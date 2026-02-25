import 'vuetify/styles'
import '@mdi/font/css/materialdesignicons.css'
import { createVuetify } from 'vuetify'
import type { ThemeDefinition } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createVueI18nAdapter } from 'vuetify/locale/adapters/vue-i18n'
import { useI18n } from 'vue-i18n'
import i18n from './i18n'

export const lightTheme: ThemeDefinition = {
  dark: false,
  colors: {
    primary: '#009688',
    secondary: '#ef5350',
    accent: '#424242',
    background: '#EEEEEE',
    success: '#69F0AE',
    warning: '#FFC107',
    error: '#DD2C00',
    info: '#4DB6AC',
  },
}

export const darkTheme: ThemeDefinition = {
  dark: true,
  colors: {
    primary: '#B2DFDB',
    secondary: '#ef5350',
    accent: '#424242',
    background: '#555555',
    surface: '#333333',
    success: '#69F0AE',
    warning: '#FFC107',
    error: '#DD2C00',
    info: '#4DB6AC',
  },
}

export function getDefaultTheme(): string {
  return localStorage.getItem('roomler-theme') || 'light'
}

const vuetify = createVuetify({
  components,
  directives,
  locale: {
    adapter: createVueI18nAdapter({ i18n, useI18n }),
  },
  theme: {
    defaultTheme: getDefaultTheme(),
    themes: {
      light: lightTheme,
      dark: darkTheme,
    },
  },
  defaults: {
    VBtn: { variant: 'flat' },
    VTextField: { variant: 'outlined', density: 'comfortable' },
    VSelect: { variant: 'outlined', density: 'comfortable' },
  },
})

export default vuetify
