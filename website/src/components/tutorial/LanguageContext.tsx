import { createContext, useContext, useState, type ReactNode } from 'react'

export type Lang = 'en' | 'ko'

interface LanguageContextType {
  lang: Lang
  setLang: (lang: Lang) => void
  t: {
    (en: string, ko: string): string
    (en: ReactNode, ko: ReactNode): ReactNode
  }
}

const LanguageContext = createContext<LanguageContextType | null>(null)

const STORAGE_KEY = 'tutorial-lang'

export function LanguageProvider({ children }: { children: ReactNode }) {
  const [lang, setLangState] = useState<Lang>(() => {
    try {
      const saved = localStorage.getItem(STORAGE_KEY)
      if (saved === 'en' || saved === 'ko') return saved
    } catch {}
    const browserLang = navigator.language || navigator.languages?.[0] || ''
    return browserLang.startsWith('ko') ? 'ko' : 'en'
  })

  const setLang = (newLang: Lang) => {
    setLangState(newLang)
    try {
      localStorage.setItem(STORAGE_KEY, newLang)
    } catch {}
  }

  const t = ((en: any, ko: any) => (lang === 'en' ? en : ko)) as LanguageContextType['t']

  return (
    <LanguageContext.Provider value={{ lang, setLang, t }}>
      {children}
    </LanguageContext.Provider>
  )
}

export function useLanguage() {
  const ctx = useContext(LanguageContext)
  if (!ctx) throw new Error('useLanguage must be used within LanguageProvider')
  return ctx
}
