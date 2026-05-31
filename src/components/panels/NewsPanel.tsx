import { useNews } from '../../hooks/useNews'

export function NewsPanel({ symbol }: { symbol?: string }) {
  const news = useNews(symbol)

  if (news.length === 0) {
    return <div className="p-3 text-xs text-terminal-muted">Keine Nachrichten.</div>
  }

  return (
    <ul className="divide-y divide-terminal-border/60">
      {news.map((n) => (
        <li key={n.id} className="px-3 py-2 hover:bg-white/5">
          <div className="text-sm leading-snug">{n.headline}</div>
          <div className="mt-1 flex items-center gap-2 text-[11px] text-terminal-muted">
            <span className="text-terminal-accent">{n.symbols.join(' ')}</span>
            <span>{n.source}</span>
            <span>{timeAgo(n.ts)}</span>
          </div>
        </li>
      ))}
    </ul>
  )
}

function timeAgo(ts: number): string {
  const s = Math.floor((Date.now() - ts) / 1000)
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}h`
  return `${Math.floor(h / 24)}d`
}
