import { useQuote } from '../hooks/useQuote'

const TAPE = ['AAPL', 'MSFT', 'NVDA', 'TSLA', 'AMZN', 'GOOGL', 'META', 'JPM', 'BTC', 'ETH', 'SPY', 'QQQ']

export function TickerTape() {
  return (
    <div className="overflow-hidden border-b border-terminal-border bg-terminal-panel">
      {/* duplicated list so the marquee loops seamlessly */}
      <div className="flex w-max animate-scroll whitespace-nowrap">
        {[...TAPE, ...TAPE].map((s, i) => (
          <TapeItem key={`${s}-${i}`} symbol={s} />
        ))}
      </div>
    </div>
  )
}

function TapeItem({ symbol }: { symbol: string }) {
  const q = useQuote(symbol)
  if (!q) return null
  const up = q.change >= 0
  return (
    <span className="inline-flex items-center gap-1.5 px-4 py-1 text-xs tnum">
      <span className="font-semibold text-terminal-text">{symbol}</span>
      <span>{q.last.toFixed(2)}</span>
      <span className={up ? 'text-terminal-up' : 'text-terminal-down'}>
        {up ? '▲' : '▼'} {q.changePct.toFixed(2)}%
      </span>
    </span>
  )
}
