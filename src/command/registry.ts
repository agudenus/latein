export type PanelType = 'quote' | 'chart' | 'news' | 'watchlist' | 'help'

export interface CommandFunc {
  code: string
  name: string
  /** panel opened by this function, if any */
  panel?: PanelType
  /** non-panel side effect */
  action?: 'add-watchlist'
  needsSymbol: boolean
}

export const FUNCTIONS: CommandFunc[] = [
  { code: 'DES', name: 'Description / Quote', panel: 'quote', needsSymbol: true },
  { code: 'GP', name: 'Price Chart', panel: 'chart', needsSymbol: true },
  { code: 'GIP', name: 'Intraday Chart', panel: 'chart', needsSymbol: true },
  { code: 'CN', name: 'Company News', panel: 'news', needsSymbol: true },
  { code: 'N', name: 'News', panel: 'news', needsSymbol: false },
  { code: 'TOP', name: 'Top News', panel: 'news', needsSymbol: false },
  { code: 'W', name: 'Add to Watchlist', action: 'add-watchlist', needsSymbol: true },
  { code: 'WATCH', name: 'Watchlist', panel: 'watchlist', needsSymbol: false },
  { code: 'HELP', name: 'Help', panel: 'help', needsSymbol: false },
  { code: 'H', name: 'Help', panel: 'help', needsSymbol: false },
]

const MAP = new Map(FUNCTIONS.map((f) => [f.code, f]))

export function lookupFunc(code: string): CommandFunc | undefined {
  return MAP.get(code.toUpperCase())
}
