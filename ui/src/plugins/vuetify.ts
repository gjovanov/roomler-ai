import 'vuetify/styles'
import '@mdi/font/css/materialdesignicons.css'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createVueI18nAdapter } from 'vuetify/locale/adapters/vue-i18n'
import { useI18n } from 'vue-i18n'
import i18n from './i18n'

const vuetify = createVuetify({
  components,
  directives,
  locale: {
    adapter: createVueI18nAdapter({ i18n, useI18n }),
  },
  theme: {
    defaultTheme: 'dark',
    themes: {
      dark: {
        dark: true,
        colors: {
          primary: '#4DB6AC',
          secondary: '#ef5350',
          accent: '#616161',
          surface: '#1E1E1E',
          background: '#121212',
          success: '#69F0AE',
          warning: '#FFC107',
          error: '#FF3D00',
          info: '#26a69a',
        },
      },
      light: {
        dark: false,
        colors: {
          primary: '#009688',
          secondary: '#ef5350',
          accent: '#424242',
          success: '#69F0AE',
          warning: '#FFC107',
          error: '#FF3D00',
          info: '#26a69a',
        },
      },
    },
  },
  defaults: {
    VBtn: { variant: 'flat' },
    VTextField: { variant: 'outlined', density: 'comfortable' },
    VSelect: { variant: 'outlined', density: 'comfortable' },
  },
})

export default vuetify
