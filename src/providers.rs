//! Package-owned players share one explicitly selected media provider.
use super::*;
const LEASE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Pause,
    Resume,
    Next,
    Previous,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Playback {
    Unknown,
    Paused,
    Playing,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    pub label: String,
    pub actions: Vec<Action>,
}
impl Declaration {
    pub fn validate(&self) -> Result<()> {
        if self.label.trim().is_empty()
            || self.label.len() > 80
            || self.label.chars().any(char::is_control)
        {
            bail!("media provider label must contain 1..80 bytes without control characters");
        }
        if !self.actions.contains(&Action::Pause) || self.actions.len() > 4 {
            bail!("media provider must support pause and at most four actions");
        }
        for (i, action) in self.actions.iter().enumerate() {
            if self.actions[..i].contains(action) {
                bail!("duplicate media action");
            }
        }
        Ok(())
    }
}
struct Provider {
    token: String,
    declaration: Declaration,
    state: Playback,
    seen: Instant,
    active: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct CommandDelivery {
    id: String,
    action: Action,
    expires_in_ms: u64,
}
struct Pending {
    id: String,
    recipient: String,
    token: String,
    action: Action,
    target: Option<(String, String)>,
    deadline: Instant,
    delivered: bool,
}
#[derive(Debug, Clone, Serialize)]
struct Outcome {
    id: String,
    success: bool,
    error: Option<&'static str>,
}
#[derive(Default)]
pub struct Broker {
    providers: HashMap<String, Provider>,
    selected: Option<String>,
    pending: Option<Pending>,
    last_result: Option<Outcome>,
}
impl Broker {
    fn expire(&mut self, now: Instant) {
        if self.pending.as_ref().is_some_and(|p| now >= p.deadline) {
            self.fail("provider command timed out");
        }
    }
    fn fail(&mut self, error: &'static str) {
        if let Some(p) = self.pending.take() {
            if let Some(provider) = self.providers.get_mut(&p.recipient) {
                provider.state = Playback::Unknown;
            }
            self.last_result = Some(Outcome {
                id: p.id,
                success: false,
                error: Some(error),
            });
        }
    }
    fn report(
        &mut self,
        id: &str,
        token: &str,
        declaration: Declaration,
        state: Playback,
        now: Instant,
    ) -> Result<()> {
        declaration.validate()?;
        self.expire(now);
        let restarted = self.providers.get(id).is_some_and(|p| p.token != token);
        if restarted
            && self
                .pending
                .as_ref()
                .is_some_and(|p| p.recipient == id || p.target.as_ref().is_some_and(|t| t.0 == id))
        {
            self.fail("provider invocation changed");
        }
        self.providers.insert(
            id.into(),
            Provider {
                token: token.into(),
                declaration,
                state,
                seen: now,
                active: true,
            },
        );
        Ok(())
    }
    fn ready(&self, id: &str, now: Instant) -> Result<&Provider> {
        let p = self
            .providers
            .get(id)
            .context("provider has not reported state")?;
        if !p.active || now.duration_since(p.seen) >= LEASE {
            bail!("provider is unavailable");
        }
        Ok(p)
    }
    fn idle_operation(&mut self, now: Instant) -> Result<()> {
        self.expire(now);
        if self.pending.is_some() {
            bail!("media operation already in progress");
        }
        Ok(())
    }
    fn select(&mut self, target: &str, now: Instant) -> Result<String> {
        self.idle_operation(now)?;
        let candidate = self.ready(target, now)?;
        if self.selected.as_deref() == Some(target) {
            return Ok(self.complete());
        }
        if candidate.state != Playback::Paused {
            bail!("target must report paused before selection");
        }
        let target_token = candidate.token.clone();
        if let Some(old) = self.selected.clone() {
            let previous = self.ready(&old, now)?;
            match previous.state {
                Playback::Unknown => bail!("current playback state is unknown"),
                Playback::Playing => {
                    return self.enqueue(
                        &old,
                        Action::Pause,
                        Some((target.into(), target_token)),
                        now,
                    )
                }
                Playback::Paused => (),
            }
        }
        self.selected = Some(target.into());
        Ok(self.complete())
    }
    fn complete(&mut self) -> String {
        let id = Uuid::new_v4().to_string();
        self.last_result = Some(Outcome {
            id: id.clone(),
            success: true,
            error: None,
        });
        id
    }
    fn command(&mut self, action: Action, now: Instant) -> Result<String> {
        self.idle_operation(now)?;
        let selected = self
            .selected
            .clone()
            .context("no media provider selected")?;
        self.enqueue(&selected, action, None, now)
    }
    fn enqueue(
        &mut self,
        recipient: &str,
        action: Action,
        target: Option<(String, String)>,
        now: Instant,
    ) -> Result<String> {
        let p = self.ready(recipient, now)?;
        if !p.declaration.actions.contains(&action) {
            bail!("provider does not support this action");
        }
        if p.state == Playback::Unknown && action != Action::Pause {
            bail!("playback outcome is unknown; reconcile state first");
        }
        let token = p.token.clone();
        let id = Uuid::new_v4().to_string();
        self.pending = Some(Pending {
            id: id.clone(),
            recipient: recipient.into(),
            token,
            action,
            target,
            deadline: now + LEASE,
            delivered: false,
        });
        Ok(id)
    }
    fn poll(&mut self, id: &str, token: &str, now: Instant) -> Result<Option<CommandDelivery>> {
        self.expire(now);
        let Some(p) = self.pending.as_mut() else {
            return Ok(None);
        };
        if p.recipient != id {
            return Ok(None);
        }
        if p.token != token {
            bail!("command belongs to a different invocation");
        }
        // Delivery is once only. A lost response has an unknown outcome; never replay it.
        if p.delivered {
            return Ok(None);
        }
        p.delivered = true;
        self.providers
            .get_mut(id)
            .context("provider disappeared")?
            .state = Playback::Unknown;
        Ok(Some(CommandDelivery {
            id: p.id.clone(),
            action: p.action,
            expires_in_ms: p.deadline.duration_since(now).as_millis() as u64,
        }))
    }
    fn ack(
        &mut self,
        id: &str,
        token: &str,
        command_id: &str,
        success: bool,
        state: Playback,
        now: Instant,
    ) -> Result<()> {
        self.expire(now);
        let p = self.pending.as_ref().context("no pending media command")?;
        if p.recipient != id || p.token != token || p.id != command_id || !p.delivered {
            bail!("result does not match delivered command");
        }
        if !success {
            self.fail("provider rejected command");
            return Ok(());
        }
        let valid_state = match p.action {
            Action::Pause => state == Playback::Paused,
            Action::Resume => state == Playback::Playing,
            Action::Next | Action::Previous => state != Playback::Unknown,
        };
        if !valid_state {
            self.fail("provider did not confirm requested state");
            bail!("unexpected playback state");
        }
        if let Some((target, token)) = &p.target {
            if !self
                .ready(target, now)
                .is_ok_and(|t| t.token == *token && t.state == Playback::Paused)
            {
                self.fail("target changed or became unavailable");
                bail!("target changed or became unavailable");
            }
        }
        let p = self.pending.take().unwrap();
        let provider = self.providers.get_mut(id).context("provider disappeared")?;
        provider.state = state;
        provider.seen = now;
        if let Some((target, _)) = p.target {
            self.selected = Some(target);
        }
        self.last_result = Some(Outcome {
            id: p.id,
            success: true,
            error: None,
        });
        Ok(())
    }
    fn snapshot(&mut self, tokens: &HashMap<String, String>, now: Instant) -> serde_json::Value {
        self.expire(now);
        let mut list: Vec<_> = self.providers.iter().map(|(id, p)| serde_json::json!({
            "id": id, "label": p.declaration.label, "actions": p.declaration.actions,
            "available": now.duration_since(p.seen) < LEASE && tokens.get(id) == Some(&p.token),
            "state": if now.duration_since(p.seen) < LEASE && tokens.get(id) == Some(&p.token) { p.state } else { Playback::Unknown }
        })).collect();
        list.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        serde_json::json!({"selected": self.selected, "providers": list,
            "pending": self.pending.as_ref().map(|p| serde_json::json!({"id": p.id,"provider":p.recipient,"action":p.action})),
            "last_result": self.last_result})
    }
    fn reconcile_tokens(&mut self, tokens: &HashMap<String, String>) {
        if self.pending.as_ref().is_some_and(|p| {
            tokens.get(&p.recipient) != Some(&p.token)
                || p.target
                    .as_ref()
                    .is_some_and(|(id, t)| tokens.get(id) != Some(t))
        }) {
            self.fail("provider stopped or restarted");
        }
        for (id, p) in &mut self.providers {
            if tokens.get(id) != Some(&p.token) {
                p.state = Playback::Unknown;
                p.active = false;
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    provider: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transport {
    action: Action,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    state: Playback,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandResult {
    id: String,
    success: bool,
    state: Playback,
}

pub async fn status(State(a): State<App>) -> Json<serde_json::Value> {
    let tokens = a.runtime_tokens.lock().await;
    let mut b = a.providers.lock().await;
    b.reconcile_tokens(&tokens);
    Json(b.snapshot(&tokens, Instant::now()))
}
pub async fn select(
    State(a): State<App>,
    Json(req): Json<Selection>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let tokens = a.runtime_tokens.lock().await;
    let mut b = a.providers.lock().await;
    b.reconcile_tokens(&tokens);
    let id = b.select(&req.provider, Instant::now())?;
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({"id":id}))))
}
pub async fn transport(
    State(a): State<App>,
    Json(req): Json<Transport>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let tokens = a.runtime_tokens.lock().await;
    let mut b = a.providers.lock().await;
    b.reconcile_tokens(&tokens);
    let id = b.command(req.action, Instant::now())?;
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({"id":id}))))
}
async fn declaration(a: &App, id: &str, headers: &HeaderMap) -> Result<Declaration, ApiError> {
    runtime_permission(a, id, headers, "media.provider").await?;
    Ok(a.plugins
        .lock()
        .await
        .manifest(id)?
        .media_provider
        .context("package does not declare media_provider")?)
}
fn check_token(tokens: &HashMap<String, String>, id: &str, headers: &HeaderMap) -> Result<String> {
    let token = runtime_token(headers)?;
    if tokens.get(id).map(String::as_str) != Some(token) {
        bail!("invalid runtime token");
    }
    Ok(token.into())
}
pub async fn report(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(req): Json<Report>,
) -> Result<StatusCode, ApiError> {
    let config = declaration(&a, &id, &headers).await?;
    let tokens = a.runtime_tokens.lock().await;
    let token = check_token(&tokens, &id, &headers)?;
    a.providers
        .lock()
        .await
        .report(&id, &token, config, req.state, Instant::now())?;
    Ok(StatusCode::NO_CONTENT)
}
pub async fn poll(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<Option<CommandDelivery>>, ApiError> {
    declaration(&a, &id, &headers).await?;
    let tokens = a.runtime_tokens.lock().await;
    let token = check_token(&tokens, &id, &headers)?;
    let mut b = a.providers.lock().await;
    b.reconcile_tokens(&tokens);
    Ok(Json(b.poll(&id, &token, Instant::now())?))
}
pub async fn result(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(req): Json<CommandResult>,
) -> Result<StatusCode, ApiError> {
    declaration(&a, &id, &headers).await?;
    let tokens = a.runtime_tokens.lock().await;
    let token = check_token(&tokens, &id, &headers)?;
    let mut b = a.providers.lock().await;
    b.reconcile_tokens(&tokens);
    b.ack(&id, &token, &req.id, req.success, req.state, Instant::now())?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_requires_pause_ack_and_does_not_play_target() {
        let mut b = Broker::default();
        let now = Instant::now();
        b.report(
            "radio",
            "r1",
            declaration(vec![Action::Pause, Action::Resume]),
            Playback::Paused,
            now,
        )
        .unwrap();
        b.report(
            "player",
            "p1",
            declaration(vec![Action::Pause, Action::Resume, Action::Next]),
            Playback::Paused,
            now,
        )
        .unwrap();
        b.select("radio", now).unwrap();
        b.report(
            "radio",
            "r1",
            declaration(vec![Action::Pause, Action::Resume]),
            Playback::Playing,
            now,
        )
        .unwrap();
        let id = b.select("player", now).unwrap();
        assert_eq!(b.selected.as_deref(), Some("radio"));
        assert!(b.poll("player", "p1", now).unwrap().is_none());
        let cmd = b.poll("radio", "r1", now).unwrap().unwrap();
        assert_eq!(cmd.id, id);
        assert_eq!(cmd.action, Action::Pause);
        b.ack("radio", "r1", &id, true, Playback::Paused, now)
            .unwrap();
        assert_eq!(b.selected.as_deref(), Some("player"));
        assert!(b.poll("player", "p1", now).unwrap().is_none());
    }
    fn declaration(actions: Vec<Action>) -> Declaration {
        Declaration {
            label: "Player".into(),
            actions,
        }
    }
    fn pair() -> (Broker, Instant) {
        let mut b = Broker::default();
        let now = Instant::now();
        for (id, token) in [("radio", "r1"), ("player", "p1")] {
            b.report(
                id,
                token,
                declaration(vec![Action::Pause, Action::Resume]),
                Playback::Paused,
                now,
            )
            .unwrap();
        }
        b.select("radio", now).unwrap();
        (b, now)
    }
    #[test]
    fn revoked_invocation_cannot_be_selected_even_if_recently_paused() {
        let (mut b, now) = pair();
        b.reconcile_tokens(&HashMap::from([("radio".into(), "r1".into())]));
        assert!(b.select("player", now).is_err());
        assert!(b.select("radio", now).is_ok());
        b.reconcile_tokens(&HashMap::new());
        assert!(b.select("radio", now).is_err());
    }
    #[test]
    fn timeout_keeps_owner_and_rejects_late_ack() {
        let (mut b, now) = pair();
        let id = b.command(Action::Resume, now).unwrap();
        b.poll("radio", "r1", now).unwrap();
        assert!(b
            .ack("radio", "r1", &id, true, Playback::Playing, now + LEASE)
            .is_err());
        assert_eq!(b.selected.as_deref(), Some("radio"));
        assert_eq!(b.providers["radio"].state, Playback::Unknown);
        assert!(!b.last_result.as_ref().unwrap().success);
    }
    #[test]
    fn result_requires_delivery_and_exact_invocation_and_id() {
        let (mut b, now) = pair();
        let id = b.command(Action::Resume, now).unwrap();
        assert!(b
            .ack("radio", "r1", &id, true, Playback::Playing, now)
            .is_err());
        assert!(b.poll("radio", "r2", now).is_err());
        b.poll("radio", "r1", now).unwrap().unwrap();
        assert!(b.poll("radio", "r1", now).unwrap().is_none());
        assert!(b
            .ack("radio", "r2", &id, true, Playback::Playing, now)
            .is_err());
        assert!(b
            .ack("radio", "r1", "old", true, Playback::Playing, now)
            .is_err());
        b.ack("radio", "r1", &id, true, Playback::Playing, now)
            .unwrap();
        assert!(b
            .ack("radio", "r1", &id, true, Playback::Playing, now)
            .is_err());
    }
    #[test]
    fn unsupported_actions_busy_and_target_restart_are_safe() {
        let (mut b, now) = pair();
        assert!(b.command(Action::Next, now).is_err());
        let id = b.command(Action::Resume, now).unwrap();
        assert!(b.select("player", now).is_err());
        b.poll("radio", "r1", now).unwrap();
        b.ack("radio", "r1", &id, true, Playback::Playing, now)
            .unwrap();
        let switch = b.select("player", now).unwrap();
        b.poll("radio", "r1", now).unwrap();
        b.report(
            "player",
            "p2",
            declaration(vec![Action::Pause]),
            Playback::Paused,
            now,
        )
        .unwrap();
        assert!(b
            .ack("radio", "r1", &switch, true, Playback::Paused, now)
            .is_err());
        assert_eq!(b.selected.as_deref(), Some("radio"));
        assert!(!b.last_result.as_ref().unwrap().success);
    }
    #[test]
    fn failed_pause_cannot_select_another_provider() {
        let (mut b, now) = pair();
        b.report(
            "radio",
            "r1",
            declaration(vec![Action::Pause]),
            Playback::Playing,
            now,
        )
        .unwrap();
        let switch = b.select("player", now).unwrap();
        b.poll("radio", "r1", now).unwrap();
        assert!(b
            .ack("radio", "r1", &switch, true, Playback::Playing, now)
            .is_err());
        assert_eq!(b.selected.as_deref(), Some("radio"));
        assert!(b.select("player", now).is_err());
    }
    #[test]
    fn missing_paused_state_and_stale_heartbeat_reject_selection() {
        let (mut b, now) = pair();
        assert!(b.select("missing", now).is_err());
        assert!(b.select("player", now + LEASE).is_err());
        b.report(
            "player",
            "p1",
            declaration(vec![Action::Pause]),
            Playback::Playing,
            now,
        )
        .unwrap();
        assert!(b.select("player", now).is_err());
    }
    #[test]
    fn invalid_declarations_are_rejected() {
        assert!(declaration(vec![Action::Resume]).validate().is_err());
        assert!(declaration(vec![Action::Pause, Action::Pause])
            .validate()
            .is_err());
        assert!(Declaration {
            label: "\n".into(),
            actions: vec![Action::Pause]
        }
        .validate()
        .is_err());
    }
}
