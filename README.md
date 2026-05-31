# LATEIN Terminal

Ein Bloomberg-inspiriertes Finanz-Terminal als Web-App (React + Vite + TypeScript).
Kommando-getriebene Bedienung, Multi-Panel-Layout, simulierte Echtzeit-Marktdaten —
**ohne** Chat.

> Hinweis: Die Marktdaten in v1 sind **synthetisch** (clientseitiger Mock mit
> Random-Walk). Die `DataProvider`-Abstraktion (`src/data/DataProvider.ts`) ist so
> ausgelegt, dass später eine echte Marktdaten-API eingehängt werden kann, ohne die
> UI anzufassen.

## Schnellstart

```bash
npm install
npm run dev      # Dev-Server (Vite) starten
npm run build    # Typecheck + Produktions-Build
npm run preview  # Produktions-Build lokal ansehen
```

Danach die angezeigte URL (Standard: http://localhost:5173) im Browser öffnen.

## Bedienung

In der Command-Leiste oben: `TICKER FUNKTION` eingeben und **Enter** drücken (= `GO`).
Mit `/` springt der Fokus jederzeit zurück in die Command-Leiste.

| Funktion | Bedeutung           | Beispiel   |
| -------- | ------------------- | ---------- |
| `DES`    | Beschreibung / Kurs | `AAPL`     |
| `GP`     | Kurs-Chart          | `AAPL GP`  |
| `GIP`    | Intraday-Chart      | `NVDA GIP` |
| `N`/`TOP`| Nachrichten         | `MSFT N`   |
| `CN`     | Firmen-News         | `TSLA CN`  |
| `W`      | zur Watchlist       | `TSLA W`   |
| `WATCH`  | Watchlist öffnen    | `WATCH`    |
| `HELP`   | Hilfe / Funktionen  | `HELP`     |

Nur einen Ticker eingeben (`AAPL`) öffnet standardmäßig das Quote-Panel (`DES`).
Bloomberg-Schreibweisen wie `AAPL US Equity GP` werden ebenfalls erkannt.

## Architektur

```
src/
  data/         DataProvider-Interface + MockProvider (Random-Walk-Engine), Seed-Daten
  hooks/        useQuote / useCandles / useNews / useWatchlist
  command/      parser.ts (Befehlssyntax) + registry.ts (Funktionscodes -> Panels)
  workspace/    Workspace-Store (offene Panels, Fokus)
  components/   CommandBar, TickerTape, Panel, Workspace
    panels/     Quote / Chart / News / Watchlist / Help
```

Charts: [`lightweight-charts`](https://github.com/tradingview/lightweight-charts).
Styling: Tailwind CSS mit dunklem, Bloomberg-inspiriertem Theme.

## Verfügbare Ticker (Demo)

AAPL, MSFT, NVDA, TSLA, AMZN, GOOGL, META, JPM, V, XOM, BTC, ETH, SPY, QQQ
