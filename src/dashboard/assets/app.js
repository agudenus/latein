/* polyarb soak monitor — the whole client.
 *
 * Deliberately small and deliberately dumb: poll /api/state, write the values that
 * changed into the cells that hold them, and otherwise leave the server-rendered page
 * alone. There is no framework, no build step and no state of its own.
 *
 * Three rules it must not break:
 *
 *  1. MONEY IS NEVER PARSED. Every monetary value arrives as a string the server already
 *     formatted from a Decimal. This file assigns those strings to textContent and never
 *     calls Number(), parseFloat() or Math.* on one. Sorting is the server's job too.
 *  2. ROWS ARE NOT REMOUNTED. An existing opportunity row is updated cell by cell; only a
 *     genuinely new id creates an element. Nothing re-keys, so nothing flickers.
 *  3. NOTHING HERE CAN ACT. It issues one GET and writes text. There is no control on
 *     this page because the daemon has nothing to control.
 */
(function () {
  'use strict';

  var pollMs = parseInt(document.documentElement.getAttribute('data-poll-ms'), 10) || 3000;
  var reduceMotion =
    window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  /* Cap simultaneous flashes: a burst of forty is a strobe, not a signal. */
  var FLASH_BUDGET = 8;

  /* ---- clock ------------------------------------------------------------------ */

  function tickClock() {
    var el = document.getElementById('clock');
    if (!el) return;
    var d = new Date();
    var p = function (n) {
      return n < 10 ? '0' + n : '' + n;
    };
    el.textContent = p(d.getUTCHours()) + ':' + p(d.getUTCMinutes()) + ':' + p(d.getUTCSeconds());
  }
  tickClock();
  setInterval(tickClock, 1000);

  /* ---- flatten the snapshot into the dotted keys the markup uses --------------- */

  function flatten(value, prefix, out) {
    if (value === null || value === undefined) return out;
    if (typeof value !== 'object') {
      out[prefix] = String(value);
      return out;
    }
    if (Array.isArray(value)) return out;
    for (var k in value) {
      if (!Object.prototype.hasOwnProperty.call(value, k)) continue;
      flatten(value[k], prefix ? prefix + '.' + k : k, out);
    }
    return out;
  }

  function keysFrom(state) {
    var out = flatten(state, '', {});
    /* Funnel stages and opportunities are arrays; give them stable dotted keys. */
    (state.funnel.stages || []).forEach(function (s) {
      out['funnel.' + s.key + '.count'] = s.count === null ? '—' : String(s.count);
      out['funnel.' + s.key + '.pct'] = s.pct === null ? '—' : s.pct;
      if (s.bar_pct !== null) out['funnel.' + s.key + '.bar_pct'] = s.bar_pct;
    });
    (state.pipeline || []).forEach(function (row) {
      out['pipeline.' + row.name.toLowerCase().replace(/ /g, '_')] = row.value;
    });
    (state.opportunities || []).forEach(function (o) {
      out['opp.' + o.id + '.gross_pct'] = o.gross_pct;
      out['opp.' + o.id + '.spread_pct'] = o.spread_pct === null ? '—' : o.spread_pct;
      out['opp.' + o.id + '.net_maker_bps'] = o.net_maker_bps === null ? '—' : o.net_maker_bps;
      out['opp.' + o.id + '.net_taker_bps'] = o.net_taker_bps;
      out['opp.' + o.id + '.size_usd'] = o.size_usd;
      out['opp.' + o.id + '.verdict_display'] = o.verdict_display;
    });
    /* Rendered with their own units alongside; keep the markup's shape. */
    out['header.refresh_secs'] = state.header.refresh_secs + 's';
    return out;
  }

  function applyKeys(map) {
    var flashed = 0;
    document.querySelectorAll('[data-key]').forEach(function (el) {
      var key = el.getAttribute('data-key');
      if (!(key in map)) return;
      var next = map[key];
      /* Bars carry their value as a width, not as text. */
      if (el.classList.contains('fbar')) {
        if (el.style.width !== next + '%') el.style.width = next + '%';
        return;
      }
      if (el.textContent === next) return;
      el.textContent = next;
      var row = el.closest('.trow');
      if (row && !reduceMotion && flashed < FLASH_BUDGET) {
        flashed += 1;
        row.classList.remove('flash');
        /* Force a reflow so the animation restarts on a row that just flashed. */
        void row.offsetWidth;
        row.classList.add('flash');
      }
    });
  }

  /* ---- lists ------------------------------------------------------------------ */

  function el(tag, cls, text) {
    var node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text !== undefined && text !== null) node.textContent = text;
    return node;
  }

  /* Mirrors render.rs's row markup. Only ever called for an id the page has not seen. */
  function buildOppRow(o) {
    var row = el('div', 'trow num');
    row.setAttribute('data-id', o.id);

    var event = el('div', 'cell-event');
    event.title = o.event_title;
    event.appendChild(document.createTextNode(o.event_title));
    event.appendChild(el('span', 'dim', ' · ' + o.category + ' ' + o.fee_display));
    event.appendChild(document.createTextNode(' '));
    event.appendChild(el('span', 'lbl lbl-' + o.label.replace(/-/g, ''), o.label));
    row.appendChild(event);

    row.appendChild(el('div', 'det det-' + o.detector, o.detector));

    var cells = [
      ['r muted', 'gross_pct', o.gross_pct],
      ['r muted', 'spread_pct', o.spread_pct === null ? '—' : o.spread_pct],
      ['r mk', 'net_maker_bps', o.net_maker_bps === null ? '—' : o.net_maker_bps],
      [
        'r tk ' + (o.net_taker_positive ? 'pos' : 'neg'),
        'net_taker_bps',
        o.net_taker_bps
      ],
      ['r size', 'size_usd', o.size_usd]
    ];
    cells.forEach(function (c) {
      var cell = el('div', c[0], c[2]);
      cell.setAttribute('data-key', 'opp.' + o.id + '.' + c[1]);
      row.appendChild(cell);
    });

    var verdictCell = el('div', 'r');
    var verdict = el('span', 'verdict v-' + o.verdict, o.verdict_display);
    verdict.setAttribute('data-key', 'opp.' + o.id + '.verdict_display');
    verdictCell.appendChild(verdict);
    row.appendChild(verdictCell);
    return row;
  }

  function syncOpportunities(list) {
    var container = document.getElementById('opp-rows');
    if (!container) return;
    var seen = {};
    var previous = container.firstElementChild;
    /* Newest first: walk the state in order and insert anything missing at the front of
       what remains, so existing rows keep their identity and their scroll position. */
    for (var i = list.length - 1; i >= 0; i -= 1) {
      var o = list[i];
      seen[o.id] = true;
      if (container.querySelector('[data-id="' + o.id + '"]')) continue;
      var row = buildOppRow(o);
      container.insertBefore(row, container.firstChild);
      if (!reduceMotion) row.classList.add('flash');
      previous = row;
    }
    void previous;
    Array.prototype.slice.call(container.children).forEach(function (node) {
      if (!seen[node.getAttribute('data-id')]) node.remove();
    });
  }

  function renderLog(list) {
    var container = document.getElementById('log-rows');
    if (!container) return;
    var signature = list
      .map(function (l) {
        return l.time + l.kind + l.subject + l.detail;
      })
      .join('|');
    if (container.getAttribute('data-sig') === signature) return;
    container.setAttribute('data-sig', signature);
    container.textContent = '';
    list.forEach(function (l) {
      var row = el('div', 'lrow num');
      row.appendChild(el('span', 'ltime', l.time));
      var body = el('span', 'lbody');
      body.appendChild(el('span', 'lkind lk-' + l.kind, l.kind));
      body.appendChild(document.createTextNode(' '));
      body.appendChild(el('span', 'lsubj', l.subject));
      body.appendChild(document.createTextNode(' '));
      body.appendChild(el('span', 'ldetail', l.detail));
      row.appendChild(body);
      container.appendChild(row);
    });
  }

  function renderCategories(list) {
    var container = document.getElementById('category-rows');
    if (!container) return;
    var signature = list
      .map(function (c) {
        return c.name + c.rate_display + c.count + (c.median_net_maker_bps || '');
      })
      .join('|');
    if (container.getAttribute('data-sig') === signature) return;
    container.setAttribute('data-sig', signature);
    container.textContent = '';
    list.forEach(function (c) {
      var row = el('div', 'crow num');
      row.appendChild(el('span', 'cname', c.name));
      row.appendChild(el('span', 'crate', c.rate_display));
      row.appendChild(el('span', 'ccount', String(c.count)));
      row.appendChild(
        el('span', 'cbps', c.median_net_maker_bps === null ? '—' : c.median_net_maker_bps + 'bps')
      );
      container.appendChild(row);
    });
  }

  /* ---- poll ------------------------------------------------------------------- */

  function currentBreak() {
    return document.body.getAttribute('data-break') || 'none';
  }

  function apply(state) {
    var next = state.break_state ? state.break_state.reason : 'none';
    /* Entering or leaving the break state changes the whole page, not a cell: the server
       decides which screen this is, so ask it for the new one. */
    if (next !== currentBreak()) {
      window.location.reload();
      return;
    }
    if (next !== 'none') return;
    applyKeys(keysFrom(state));
    syncOpportunities(state.opportunities || []);
    renderCategories(state.categories || []);
    renderLog(state.log || []);
  }

  function poll() {
    fetch('/api/state', { headers: { accept: 'application/json' }, cache: 'no-store' })
      .then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      })
      .then(apply)
      .catch(function (err) {
        /* A failed poll is the dashboard's own problem, never the daemon's. Say so in the
           console and keep the last good page on screen rather than blanking it. */
        if (window.console) console.warn('polyarb dashboard: state poll failed', err);
      });
  }

  setInterval(poll, pollMs);
})();
