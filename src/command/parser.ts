export interface ParsedCommand {
  symbol?: string
  /** function code, defaults to DES when none is given */
  func: string
  args: string[]
  raw: string
}

// Bloomberg-style security qualifiers that we simply ignore, e.g. "AAPL US Equity".
const QUALIFIERS = new Set([
  'US', 'EQUITY', 'EQ', 'INDEX', 'CURNCY', 'COMDTY', 'CORP', 'GOVT',
  'LN', 'GR', 'UQ', 'UN', 'UW',
])

const KNOWN_FUNCS = new Set([
  'DES', 'GP', 'GIP', 'N', 'TOP', 'CN', 'W', 'WATCH', 'HELP', 'H',
])

const SYMBOL_RE = /^[A-Z][A-Z.\-]{0,9}$/

/**
 * Parse a Bloomberg-style command line such as "AAPL US Equity GP" or "MSFT N".
 * Tokens are classified as a function code, a security qualifier (ignored),
 * a ticker symbol, or trailing arguments.
 */
export function parseCommand(input: string): ParsedCommand {
  const raw = input.trim()
  const tokens = raw.split(/\s+/).filter(Boolean)

  let symbol: string | undefined
  let func: string | undefined
  const args: string[] = []

  for (const tok of tokens) {
    const up = tok.toUpperCase()
    if (KNOWN_FUNCS.has(up)) {
      func = up
    } else if (QUALIFIERS.has(up)) {
      // ignore
    } else if (!symbol && SYMBOL_RE.test(up)) {
      symbol = up
    } else {
      args.push(tok)
    }
  }

  return { symbol, func: func ?? 'DES', args, raw }
}
