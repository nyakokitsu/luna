// Luna injects this script into every plugin page (GET /web/:id).
//
// Plugin pages run in a sandboxed frame with an opaque origin, so they cannot
// call the Luna API directly. The bridge keeps `fetch('/api/...')` working:
// it hands the request to the Luna UI around the frame, which sends it with
// X-Luna-Panel; the API then allows only what this page's package may use.
// It also reports the height of `.wrap` and applies the theme the UI selects.
// See docs/WEB_PANELS.md.
(function () {
  'use strict';
  var host = window.parent;
  if (!host || host === window) return; // opened outside the Luna UI
  var pending = {};
  var sequence = 0;
  var nativeFetch = typeof window.fetch === 'function' ? window.fetch.bind(window) : null;

  function apiPath(input) {
    var raw = typeof input === 'string' ? input : input && input.url ? input.url : String(input);
    var url;
    try {
      url = new URL(raw, document.baseURI);
    } catch (_) {
      return null;
    }
    if (url.host !== location.host) return null;
    if (url.pathname !== '/api' && url.pathname.indexOf('/api/') !== 0) return null;
    return url.pathname + url.search;
  }

  function plainHeaders(headers) {
    var result = {};
    if (headers) {
      new Headers(headers).forEach(function (value, name) {
        result[name] = value;
      });
    }
    return result;
  }

  function cloneableBody(body) {
    return (
      typeof body === 'string' ||
      body instanceof ArrayBuffer ||
      ArrayBuffer.isView(body) ||
      (typeof Blob === 'function' && body instanceof Blob)
    );
  }

  window.fetch = function (input, init) {
    var path = apiPath(input);
    if (path === null) {
      return nativeFetch
        ? nativeFetch(input, init)
        : Promise.reject(new TypeError('fetch is not available'));
    }
    init = init || {};
    var request = typeof Request === 'function' && input instanceof Request ? input : null;
    var method = String(init.method || (request && request.method) || 'GET').toUpperCase();
    var headers = plainHeaders(init.headers || (request && request.headers));
    var body = init.body;
    if (typeof URLSearchParams === 'function' && body instanceof URLSearchParams) {
      if (!headers['content-type']) {
        headers['content-type'] = 'application/x-www-form-urlencoded;charset=UTF-8';
      }
      body = body.toString();
    }
    if (body != null && !cloneableBody(body)) {
      return Promise.reject(
        new TypeError('Luna panel bridge supports string, ArrayBuffer and Blob bodies')
      );
    }
    return new Promise(function (resolve, reject) {
      var id = ++sequence;
      pending[id] = { resolve: resolve, reject: reject };
      host.postMessage(
        { luna: 'fetch', id: id, path: path, method: method, headers: headers, body: body == null ? null : body },
        '*'
      );
    });
  };

  window.addEventListener('message', function (event) {
    if (event.source !== host) return;
    var message = event.data;
    if (!message || typeof message !== 'object') return;
    if (message.luna === 'fetch-result' && pending[message.id]) {
      var call = pending[message.id];
      delete pending[message.id];
      if (message.error) {
        call.reject(new TypeError(message.error));
        return;
      }
      var empty = message.status === 204 || message.status === 205 || message.status === 304;
      call.resolve(
        new Response(empty ? null : message.body, {
          status: message.status,
          statusText: message.statusText || '',
          headers: message.headers || {},
        })
      );
    } else if (message.luna === 'theme' && typeof message.theme === 'string') {
      document.documentElement.dataset.theme = message.theme;
    }
  });

  function reportSize() {
    var root = document.querySelector('.wrap');
    if (!root || typeof ResizeObserver !== 'function') return;
    var last = -1;
    function send() {
      var height = Math.ceil(root.getBoundingClientRect().height);
      if (height === last) return;
      last = height;
      host.postMessage({ luna: 'size', height: height }, '*');
    }
    new ResizeObserver(send).observe(root);
    send();
  }
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', reportSize);
  } else {
    reportSize();
  }
  host.postMessage({ luna: 'ready' }, '*');
})();
