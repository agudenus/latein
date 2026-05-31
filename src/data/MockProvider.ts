import type { Security, Quote, Candle, NewsItem, Range } from '../types/market'
import type { DataProvider } from './DataProvider'
import { SEED, type SeedSecurity } from './seed'

const RANGE_BARS: Record<Range, { count: number; stepSec: number }> = {
  '1D': { count: 78, stepSec: 5 * 60 },
  '5D': { count: 130, stepSec: 15 * 60 },
  '1M': { count: 22, stepSec: 24 * 3600 },
  '6M': { count: 126, stepSec: 24 * 3600 },
  '1Y': { count: 252, stepSec: 24 * 3600 },
}

const HEADLINE_TEMPLATES = [
  '{name} beats quarterly earnings estimates',
  '{sym} shares climb as analysts lift price target',
  '{name} unveils refreshed product lineup',
  '{sym} draws regulatory scrutiny in the EU',
  '{name} CEO signals confidence in full-year outlook',
  'Options activity surges in {sym} ahead of results',
  '{name} expands share buyback program',
  'Analysts split on {sym} after latest guidance',
  '{name} announces leadership reshuffle',
  '{sym} hits fresh high on sector rotation',
]

const SOURCES = ['Reuters', 'Bloomberg', 'WSJ', 'FT', 'CNBC', 'MarketWatch']

interface LiveState {
  sec: SeedSecurity
  last: number
  open: number
  high: number
  low: number
  prevClose: number
  volume: number
}

/** Deterministic PRNG (mulberry32) so generated history is stable per symbol. */
function mulberry32(seed: number): () => number {
  let a = seed >>> 0
  return function () {
    a = (a + 0x6d2b79f5) | 0
    let t = Math.imul(a ^ (a >>> 15), 1 | a)
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

function hashSymbol(s: string): number {
  let h = 2166136261
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i)
    h = Math.imul(h, 16777619)
  }
  return h >>> 0
}

function round2(n: number): number {
  return Math.round(n * 100) / 100
}

function toSecurity(s: Security): Security {
  return {
    symbol: s.symbol,
    name: s.name,
    exchange: s.exchange,
    currency: s.currency,
    sector: s.sector,
  }
}

export class MockProvider implements DataProvider {
  private state = new Map<string, LiveState>()
  private subscribers = new Map<string, Set<(q: Quote) => void>>()
  private timer: number | null = null

  constructor() {
    for (const s of SEED) {
      this.state.set(s.symbol, {
        sec: s,
        last: s.price,
        open: s.prevClose,
        high: Math.max(s.price, s.prevClose),
        low: Math.min(s.price, s.prevClose),
        prevClose: s.prevClose,
        volume: Math.floor(s.vol * 1e9 * (0.5 + Math.random())),
      })
    }
  }

  search(query: string): Security[] {
    const q = query.trim().toUpperCase()
    const list = SEED.map(toSecurity)
    if (!q) return list
    return list.filter((s) => s.symbol.includes(q) || s.name.toUpperCase().includes(q))
  }

  getSecurity(symbol: string): Security | undefined {
    const st = this.state.get(symbol.toUpperCase())
    return st ? toSecurity(st.sec) : undefined
  }

  subscribeQuote(symbol: string, cb: (q: Quote) => void): () => void {
    const sym = symbol.toUpperCase()
    if (!this.state.has(sym)) return () => {}

    let set = this.subscribers.get(sym)
    if (!set) {
      set = new Set()
      this.subscribers.set(sym, set)
    }
    set.add(cb)
    cb(this.quoteOf(this.state.get(sym)!)) // emit current value immediately
    this.ensureTimer()

    return () => {
      const current = this.subscribers.get(sym)
      if (!current) return
      current.delete(cb)
      if (current.size === 0) this.subscribers.delete(sym)
      this.maybeStopTimer()
    }
  }

  getCandles(symbol: string, range: Range): Candle[] {
    const st = this.state.get(symbol.toUpperCase())
    if (!st) return []

    const { count, stepSec } = RANGE_BARS[range]
    const rand = mulberry32(hashSymbol(symbol.toUpperCase()) ^ Math.imul(count, 2654435761))
    const now = Math.floor(Date.now() / 1000)

    // Random-walk backwards from the current price, then output chronologically.
    const closes: number[] = []
    let price = st.last
    for (let i = 0; i < count; i++) {
      closes.push(price)
      price = Math.max(0.5, price - (rand() - 0.5) * 2 * st.sec.vol * price)
    }
    closes.reverse()

    const candles: Candle[] = []
    for (let i = 0; i < count; i++) {
      const close = closes[i]
      const open = i === 0 ? close * (1 + (rand() - 0.5) * 0.01) : closes[i - 1]
      const high = Math.max(open, close) * (1 + rand() * st.sec.vol * 0.5)
      const low = Math.min(open, close) * (1 - rand() * st.sec.vol * 0.5)
      candles.push({
        time: now - (count - 1 - i) * stepSec,
        open: round2(open),
        high: round2(high),
        low: round2(low),
        close: round2(close),
        volume: Math.floor(1e6 * (0.5 + rand())),
      })
    }
    return candles
  }

  getNews(symbol?: string): NewsItem[] {
    const symbols = symbol ? [symbol.toUpperCase()] : SEED.map((s) => s.symbol)
    const now = Date.now()
    const items: NewsItem[] = []
    let idx = 0

    for (const sym of symbols) {
      const sec = this.state.get(sym)?.sec
      if (!sec) continue
      const rand = mulberry32(hashSymbol(sym) + (symbol ? 7 : 0))
      const n = symbol ? 8 : 2
      for (let i = 0; i < n; i++) {
        const tpl = HEADLINE_TEMPLATES[Math.floor(rand() * HEADLINE_TEMPLATES.length)]
        items.push({
          id: `${sym}-${i}-${idx++}`,
          headline: tpl.replace('{name}', sec.name).replace('{sym}', sym),
          source: SOURCES[Math.floor(rand() * SOURCES.length)],
          ts: now - Math.floor(rand() * 1000 * 60 * 60 * 24),
          symbols: [sym],
          summary: `${sec.name} (${sym}) — ${sec.sector}. Automatisch generierte Demo-Meldung.`,
        })
      }
    }
    return items.sort((a, b) => b.ts - a.ts)
  }

  private quoteOf(st: LiveState): Quote {
    const change = st.last - st.prevClose
    const spread = Math.max(0.01, st.last * 0.0002)
    return {
      symbol: st.sec.symbol,
      last: round2(st.last),
      change: round2(change),
      changePct: round2((change / st.prevClose) * 100),
      bid: round2(st.last - spread),
      ask: round2(st.last + spread),
      open: round2(st.open),
      high: round2(st.high),
      low: round2(st.low),
      prevClose: round2(st.prevClose),
      volume: st.volume,
      ts: Date.now(),
    }
  }

  private ensureTimer(): void {
    if (this.timer != null) return
    this.timer = window.setInterval(() => this.tick(), 1000)
  }

  private maybeStopTimer(): void {
    if (this.subscribers.size === 0 && this.timer != null) {
      window.clearInterval(this.timer)
      this.timer = null
    }
  }

  private tick(): void {
    for (const [sym, set] of this.subscribers) {
      const st = this.state.get(sym)
      if (!st) continue
      const drift = (Math.random() - 0.5) * 2 * st.sec.vol * st.last * 0.15
      st.last = Math.max(0.01, st.last + drift)
      st.high = Math.max(st.high, st.last)
      st.low = Math.min(st.low, st.last)
      st.volume += Math.floor(Math.random() * 50000)
      const q = this.quoteOf(st)
      for (const cb of set) cb(q)
    }
  }
}
