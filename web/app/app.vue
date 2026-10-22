<script setup lang="ts">
import { en, ru } from '@nuxt/ui/locale'
import {language, t} from './utils/i18n'

useHead(() => ({title: t('Кодрик'), htmlAttrs: {lang: language.value}}))

const loginData = document.getElementById('kodrik-login')?.textContent
const login = loginData ? JSON.parse(loginData) as { returnTo: string; error: string } : null
const homeData = document.getElementById('kodrik-home')?.textContent
const home = homeData ? JSON.parse(homeData) as { prefix: string; loggedIn: boolean } : null
const documentData=document.getElementById('kodrik-document')?.textContent
const page=documentData?JSON.parse(documentData) as {url:string;chatUrl:string;loginUrl:string}:null
</script>

<template>
  <UApp :locale="language === 'ru' ? ru : en">
    <LoginPage v-if="login" :return-to="login.returnTo" :error="login.error" />
    <LazyHomePage v-else-if="home" v-bind="home" />
    <LazyDocumentPage v-else-if="page" v-bind="page" />
    <LazyChatDashboard v-else />
  </UApp>
</template>
