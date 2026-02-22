import { LanguageProvider } from '../components/tutorial/LanguageContext'
import Hero from '../components/Hero'
import TelegramShowcase from '../components/TelegramShowcase'
import Features from '../components/Features'
import AllInOne from '../components/AllInOne'
import Footer from '../components/Footer'

export default function LandingPage() {
  return (
    <LanguageProvider>
      <div className="min-h-screen bg-bg-dark overflow-x-hidden">
        <Hero />
        <TelegramShowcase />
        <Features />
        <AllInOne />
        <Footer />
      </div>
    </LanguageProvider>
  )
}
