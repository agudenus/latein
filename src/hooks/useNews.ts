import { useEffect, useState } from 'react'
import type { NewsItem } from '../types/market'
import { useProvider } from '../data/DataProvider'

export function useNews(symbol?: string): NewsItem[] {
  const provider = useProvider()
  const [news, setNews] = useState<NewsItem[]>([])

  useEffect(() => {
    setNews(provider.getNews(symbol))
  }, [provider, symbol])

  return news
}
