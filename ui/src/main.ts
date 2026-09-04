// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { createApp } from 'vue'
import i18n from '@/plugins/i18n'
import vuetify from '@/plugins/vuetify'
import pinia from '@/plugins/pinia'
import router from '@/plugins/router'
import App from '@/App.vue'
import { clearLegacyTokens } from '@/api/session'
import { useCapabilitiesStore } from '@/stores/capabilities'

// Delete the tokens earlier versions kept in localStorage — an access token
// (7 days) and, worse, a refresh token (30 days) that re-mints access tokens.
// They are cookies now, but shipping that fix only stops NEW ones being
// written: a user who never signs out again would otherwise carry the old pair
// until they expire. Runs before anything can read them.
clearLegacyTokens()

const app = createApp(App)

// Plugin order matters: i18n FIRST, vuetify SECOND
app.use(i18n)
app.use(vuetify)
app.use(pinia)
// FR-69 P9 — ask the server what it is composed of, once, before the first
// navigation resolves: the router's module guard awaits this same request,
// and the navigation hides the pillars the server does not mount. Never
// blocks the mount and never throws — an unanswered server fails OPEN.
void useCapabilitiesStore().load()
app.use(router)

app.mount('#app')
