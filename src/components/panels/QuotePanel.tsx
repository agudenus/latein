import { useProvider } from '../../data/DataProvider'
import { useQuote } from '../../hooks/useQuote'

export function QuotePanel({ symbol }: { symbol: string }) {
  const provider = useProvider()
  const sec = provider.getSecurity(symbol)
  const q = useQuote(symbol)

  if (!sec) return <Centered text={`Keine Daten für ${symbol}`} />
  if (!q) return <Centered text="Lädt…" />

  const up = q.change >= 0
  const color = up ? 'text-terminal-up' : 'text-terminal-down'

  return (
    <div className="flex h-full flex-col p-3 text-sm">
      <div className="mb-2">
        <div className="text-base font-semibold">
          {sec.symbol} <span className="font-normal text-terminal-muted">{sec.exchange}</span>
        </div>
        <div className="text-xs text-terminal-muted">
          {sec.name} · {sec.sector}
        </div>
      </div>

      <div className="mb-3 flex flex-wrap items-baseline gap-3">
        <span className={`text-3xl font-bold tnum ${color}`}>{q.last.toFixed(2)}</span>
        <span className={`tnum ${color}`}>
          {up ? '+' : ''}
          {q.change.toFixed(2)} ({up ? '+' : ''}
          {q.changePct.toFixed(2)}%)
        </span>
        <span className="text-xs text-terminal-muted">{sec.currency}</span>
      </div>

      <dl className="grid grid-cols-2 gap-x-6 gap-y-1 text-xs tnum">
        <Row label="Open" value={q.open.toFixed(2)} />
        <Row label="Prev Close" value={q.prevClose.toFixed(2)} />
        <Row label="High" value={q.high.toFixed(2)} />
        <Row label="Low" value={q.low.toFixed(2)} />
        <Row label="Bid" value={q.bid.toFixed(2)} />
        <Row label="Ask" value={q.ask.toFixed(2)} />
        <Row label="Volume" value={q.volume.toLocaleString('en-US')} />
        <Row label="Updated" value={new Date(q.ts).toLocaleTimeString()} />
      </dl>
    </div>
  )
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex justify-between border-b border-terminal-border/50 py-0.5">
      <dt className="text-terminal-muted">{label}</dt>
      <dd>{value}</dd>
    </div>
  )
}

function Centered({ text }: { text: string }) {
  return (
    <div className="flex h-full items-center justify-center text-xs text-terminal-muted">{text}</div>
  )
}
