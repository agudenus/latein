import { useWatchlist } from '../../hooks/useWatchlist'
import { useQuote } from '../../hooks/useQuote'
import { useWorkspace } from '../../workspace/workspaceStore'

export function WatchlistPanel() {
  const { list, remove } = useWatchlist()

  return (
    <table className="w-full text-xs tnum">
      <thead className="sticky top-0 bg-terminal-panel text-terminal-muted">
        <tr className="border-b border-terminal-border text-left">
          <th className="px-2 py-1 font-normal">Sym</th>
          <th className="px-2 py-1 text-right font-normal">Last</th>
          <th className="px-2 py-1 text-right font-normal">Chg%</th>
          <th className="px-2 py-1"></th>
        </tr>
      </thead>
      <tbody>
        {list.map((s) => (
          <WatchRow key={s} symbol={s} onRemove={() => remove(s)} />
        ))}
        {list.length === 0 && (
          <tr>
            <td colSpan={4} className="px-2 py-3 text-center text-terminal-muted">
              Watchlist leer. Hinzufügen mit z.B. NVDA W
            </td>
          </tr>
        )}
      </tbody>
    </table>
  )
}

function WatchRow({ symbol, onRemove }: { symbol: string; onRemove: () => void }) {
  const q = useQuote(symbol)
  const { open } = useWorkspace()
  const up = (q?.change ?? 0) >= 0

  return (
    <tr
      className="cursor-pointer border-b border-terminal-border/40 hover:bg-white/5"
      onClick={() => open({ type: 'quote', symbol, title: `${symbol} Quote` })}
    >
      <td className="px-2 py-1 font-semibold">{symbol}</td>
      <td className="px-2 py-1 text-right">{q ? q.last.toFixed(2) : '—'}</td>
      <td className={`px-2 py-1 text-right ${up ? 'text-terminal-up' : 'text-terminal-down'}`}>
        {q ? `${up ? '+' : ''}${q.changePct.toFixed(2)}%` : '—'}
      </td>
      <td className="px-2 py-1 text-right">
        <button
          onClick={(e) => {
            e.stopPropagation()
            onRemove()
          }}
          aria-label={`${symbol} entfernen`}
          className="text-terminal-muted hover:text-terminal-down"
        >
          ✕
        </button>
      </td>
    </tr>
  )
}
