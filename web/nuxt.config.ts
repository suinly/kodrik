export default defineNuxtConfig({
  compatibilityDate: '2026-10-20',
  ssr: false,
  devtools: { enabled: false },
  modules: ['@nuxt/ui', '@nuxtjs/mdc'],
  css: ['~/assets/css/main.css'],
  app: {
    // Auth and chat share the base prefix; compiled assets stay under /chat/.
    baseURL: '/__kodrik_chat__/',
    buildAssetsDir: 'chat/_nuxt/',
    head: {
      title: 'Kodrik', htmlAttrs: { lang: 'en' },
      viewport: 'width=device-width, initial-scale=1, viewport-fit=cover, interactive-widget=resizes-content',
      meta: [
        { name: 'theme-color', content: '#23273a' },
        { name: 'apple-mobile-web-app-capable', content: 'yes' },
        { name: 'apple-mobile-web-app-title', content: 'Kodrik' },
        { name: 'apple-mobile-web-app-status-bar-style', content: 'default' },
      ],
    },
  },
  ui: { fonts: false },
  icon: { provider: 'none', clientBundle: { scan: true } },
  mdc: { highlight: false },
  nitro: { output: { publicDir: 'dist' } },
})
