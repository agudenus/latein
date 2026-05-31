import { useEffect, useRef, useState } from 'react'
import {
  createChart,
  ColorType,
  type IChartApi,
  type ISeriesApi,
} from 'lightweight-charts'
import { useCandles } from '../../hooks/useCandles'
import type { Range } from '../../types/market'

const RANGES: Range[] = ['1D', '5D', '1M', '6M', '1Y']

export function ChartPanel({ symbol }: { symbol: string }) {
  const [range, setRange] = useState<Range>('1M')
  const candles = useCandles(symbol, range)
  const containerRef = useRef<HTMLDivElement>(null)
  const chartRef = useRef<IChartApi | null>(null)
  const seriesRef = useRef<ISeriesApi<'Candlestick'> | null>(null)

  // Create the chart once.
  useEffect(() => {
    const el = containerRef.current
    if (!el) return

    const chart = createChart(el, {
      autoSize: true,
      layout: {
        background: { type: ColorType.Solid, color: 'transparent' },
        textColor: '#5b6675',
      },
      grid: {
        vertLines: { color: '#1e2630' },
        horzLines: { color: '#1e2630' },
      },
      rightPriceScale: { borderColor: '#1e2630' },
      timeScale: { borderColor: '#1e2630', timeVisible: true, secondsVisible: false },
    })
    const series = chart.addCandlestickSeries({
      upColor: '#26d07c',
      downColor: '#ff4d4d',
      borderUpColor: '#26d07c',
      borderDownColor: '#ff4d4d',
      wickUpColor: '#26d07c',
      wickDownColor: '#ff4d4d',
    })

    chartRef.current = chart
    seriesRef.current = series
    return () => {
      chart.remove()
      chartRef.current = null
      seriesRef.current = null
    }
  }, [])

  // Push data whenever candles change.
  useEffect(() => {
    const series = seriesRef.current
    if (!series) return
    series.setData(
      candles.map((c) => ({
        time: c.time,
        open: c.open,
        high: c.high,
        low: c.low,
        close: c.close,
      })) as Parameters<ISeriesApi<'Candlestick'>['setData']>[0],
    )
    chartRef.current?.timeScale().fitContent()
  }, [candles])

  return (
    <div className="flex h-full flex-col">
      <div className="flex items-center gap-1 border-b border-terminal-border px-2 py-1">
        {RANGES.map((r) => (
          <button
            key={r}
            onClick={() => setRange(r)}
            className={`rounded px-1.5 py-0.5 text-xs ${
              r === range
                ? 'bg-terminal-accent text-black'
                : 'text-terminal-muted hover:text-terminal-text'
            }`}
          >
            {r}
          </button>
        ))}
        <span className="ml-auto text-xs text-terminal-muted">{symbol}</span>
      </div>
      <div ref={containerRef} className="min-h-0 flex-1" />
    </div>
  )
}
