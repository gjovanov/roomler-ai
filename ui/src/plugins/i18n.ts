// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { createI18n } from 'vue-i18n'
import { en as vuetifyEn } from 'vuetify/locale'
import en from '@/locales/en.json'

const i18n = createI18n({
  legacy: false,
  locale: 'en',
  fallbackLocale: 'en',
  messages: { en: { ...en, $vuetify: vuetifyEn } },
})

export default i18n
