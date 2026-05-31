import { FUNCTIONS } from '../../command/registry'

export function HelpPanel() {
  return (
    <div className="p-3 text-xs">
      <p className="mb-2 text-terminal-muted">
        Syntax: <span className="text-terminal-text">TICKER FUNKTION</span> + Enter (= GO).
        Beispiele: <span className="text-terminal-text">AAPL GP</span>,{' '}
        <span className="text-terminal-text">MSFT N</span>,{' '}
        <span className="text-terminal-text">TSLA W</span>. Drücke{' '}
        <span className="text-terminal-text">/</span> um die Command-Leiste zu fokussieren.
      </p>
      <table className="w-full">
        <tbody>
          {FUNCTIONS.map((f) => (
            <tr key={f.code} className="border-b border-terminal-border/40">
              <td className="py-1 pr-3 font-semibold text-terminal-accent">{f.code}</td>
              <td className="py-1 text-terminal-text">{f.name}</td>
              <td className="py-1 text-right text-terminal-muted">
                {f.needsSymbol ? 'Ticker nötig' : '—'}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}
