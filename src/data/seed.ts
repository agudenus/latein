import type { Security } from '../types/market'

export interface SeedSecurity extends Security {
  /** last reference price */
  price: number
  prevClose: number
  /** rough daily volatility used to drive the random walk */
  vol: number
}

export const SEED: SeedSecurity[] = [
  { symbol: 'AAPL', name: 'Apple Inc.', exchange: 'NASDAQ', currency: 'USD', sector: 'Technology', price: 212.45, prevClose: 210.02, vol: 0.018 },
  { symbol: 'MSFT', name: 'Microsoft Corp.', exchange: 'NASDAQ', currency: 'USD', sector: 'Technology', price: 441.18, prevClose: 444.05, vol: 0.016 },
  { symbol: 'NVDA', name: 'NVIDIA Corp.', exchange: 'NASDAQ', currency: 'USD', sector: 'Semiconductors', price: 131.26, prevClose: 128.4, vol: 0.032 },
  { symbol: 'TSLA', name: 'Tesla Inc.', exchange: 'NASDAQ', currency: 'USD', sector: 'Automotive', price: 248.5, prevClose: 252.1, vol: 0.035 },
  { symbol: 'AMZN', name: 'Amazon.com Inc.', exchange: 'NASDAQ', currency: 'USD', sector: 'Consumer Discretionary', price: 186.4, prevClose: 184.9, vol: 0.02 },
  { symbol: 'GOOGL', name: 'Alphabet Inc. Class A', exchange: 'NASDAQ', currency: 'USD', sector: 'Communication Services', price: 178.32, prevClose: 179.6, vol: 0.019 },
  { symbol: 'META', name: 'Meta Platforms Inc.', exchange: 'NASDAQ', currency: 'USD', sector: 'Communication Services', price: 502.1, prevClose: 496.8, vol: 0.024 },
  { symbol: 'JPM', name: 'JPMorgan Chase & Co.', exchange: 'NYSE', currency: 'USD', sector: 'Financials', price: 205.7, prevClose: 206.3, vol: 0.014 },
  { symbol: 'V', name: 'Visa Inc.', exchange: 'NYSE', currency: 'USD', sector: 'Financials', price: 271.9, prevClose: 270.4, vol: 0.013 },
  { symbol: 'XOM', name: 'Exxon Mobil Corp.', exchange: 'NYSE', currency: 'USD', sector: 'Energy', price: 114.2, prevClose: 115.1, vol: 0.017 },
  { symbol: 'BTC', name: 'Bitcoin / USD', exchange: 'CRYPTO', currency: 'USD', sector: 'Cryptocurrency', price: 67450, prevClose: 66100, vol: 0.04 },
  { symbol: 'ETH', name: 'Ethereum / USD', exchange: 'CRYPTO', currency: 'USD', sector: 'Cryptocurrency', price: 3520, prevClose: 3580, vol: 0.045 },
  { symbol: 'SPY', name: 'SPDR S&P 500 ETF', exchange: 'NYSE ARCA', currency: 'USD', sector: 'Index ETF', price: 546.8, prevClose: 545.2, vol: 0.009 },
  { symbol: 'QQQ', name: 'Invesco QQQ Trust', exchange: 'NASDAQ', currency: 'USD', sector: 'Index ETF', price: 478.3, prevClose: 480.1, vol: 0.011 },
]
