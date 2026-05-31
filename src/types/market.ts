export interface Security {
  symbol: string
  name: string
  exchange: string
  currency: string
  sector: string
}

export interface Quote {
  symbol: string
  last: number
  change: number
  changePct: number
  bid: number
  ask: number
  open: number
  high: number
  low: number
  prevClose: number
  volume: number
  ts: number
}

export interface Candle {
  /** unix timestamp in seconds */
  time: number
  open: number
  high: number
  low: number
  close: number
  volume: number
}

export interface NewsItem {
  id: string
  headline: string
  source: string
  ts: number
  symbols: string[]
  summary: string
}

export type Range = '1D' | '5D' | '1M' | '6M' | '1Y'
