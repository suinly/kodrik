<script setup lang="ts">
import {base, checkedFetch, login, messageOf} from '../utils/api'
import {t} from '../utils/i18n'

type Profile = {name:string;owner:boolean;telegram_id:number|null;password_set:boolean}
const profile = ref<Profile|null>(null)
const name = ref('')
const loading = ref(false), saving = ref(false), changing = ref(false)
const error = ref(''), saved = ref(false), passwordError = ref('')
const passwords = reactive({current_password:'',new_password:'',confirm_password:''})

async function request<T>(path='',method='GET',body?:unknown):Promise<T> {
  const response = await checkedFetch(new URL(`api/profile${path}`,base),{
    method,...(body===undefined?{}:{headers:{'Content-Type':'application/json'},body:JSON.stringify(body)}),
  })
  return response.status===204 ? undefined as T : response.json()
}

async function load() {
  loading.value=true;error.value=''
  try {profile.value=await request<Profile>();name.value=profile.value.name}
  catch(reason) {error.value=messageOf(reason)}
  finally {loading.value=false}
}

async function save() {
  if(saving.value || changing.value) return
  saving.value=true;error.value='';saved.value=false
  try {
    profile.value=await request<Profile>('','PATCH',{name:name.value})
    name.value=profile.value.name;saved.value=true
  } catch(reason) {error.value=messageOf(reason)}
  finally {saving.value=false}
}

async function changePassword() {
  if(changing.value || saving.value) return
  passwordError.value=''
  if([...passwords.new_password].length<15) {
    passwordError.value=t('Пароль должен содержать не менее 15 символов.');return
  }
  if(new TextEncoder().encode(passwords.new_password).length>1024) {
    passwordError.value=t('Этот пароль слишком длинный.');return
  }
  if(passwords.new_password!==passwords.confirm_password) {
    passwordError.value=t('Новый пароль и подтверждение не совпадают.');return
  }
  changing.value=true
  try {
    await request('/password','POST',{...passwords})
    Object.assign(passwords,{current_password:'',new_password:'',confirm_password:''})
    const destination=new URL(login())
    destination.searchParams.set('password_changed','1')
    window.location.assign(destination)
  } catch(reason) {passwordError.value=messageOf(reason);changing.value=false}
}

onMounted(load)
</script>

<template>
  <section :aria-label="t('Профиль')" class="mx-auto w-full max-w-3xl space-y-8 p-4 sm:p-8">
    <p v-if="loading" role="status" class="text-sm text-muted">{{ t('Загружаю профиль…') }}</p>
    <div v-if="error" class="space-y-2">
      <p role="alert" class="text-sm text-error">{{ t(error) }}</p>
      <UButton v-if="!profile" :label="t('Повторить')" color="neutral" variant="soft" :loading="loading" @click="load" />
    </div>
    <template v-if="profile">
      <form class="space-y-5" @submit.prevent="save">
        <h2 class="text-lg font-semibold text-highlighted">{{ t('Личные данные') }}</h2>
        <dl class="flex flex-wrap gap-x-12 gap-y-4 text-sm">
          <div class="space-y-1"><dt class="text-muted">{{ t('Роль') }}</dt><dd>{{ t(profile.owner?'Владелец':'Приглашённый пользователь') }}</dd></div>
          <div v-if="profile.telegram_id" class="space-y-1"><dt class="text-muted">{{ t('ID в Telegram') }}</dt><dd>{{ profile.telegram_id }}</dd></div>
        </dl>
        <UFormField :label="t('Имя')" required>
          <UInput v-model="name" :aria-label="t('Имя')" name="name" autocomplete="name" :maxlength="200" :disabled="saving || changing" required class="w-full" @update:model-value="saved=false" />
        </UFormField>
        <div class="flex flex-wrap items-center gap-3">
          <UButton type="submit" :label="t('Сохранить профиль')" :loading="saving" :disabled="changing || !name.trim()" />
          <p v-if="saved" role="status" class="text-sm text-success">{{ t('Профиль сохранён.') }}</p>
        </div>
      </form>
      <USeparator />
      <form v-if="profile.owner" class="space-y-5" @submit.prevent="changePassword">
        <div class="space-y-2">
          <h2 class="text-lg font-semibold text-highlighted">{{ t('Пароль') }}</h2>
          <p id="password-requirements" class="text-sm leading-relaxed text-muted">{{ t('Не менее 15 символов. Используйте уникальную длинную фразу; простые повторы и распространённые пароли не принимаются.') }}</p>
        </div>
        <UFormField v-if="profile.password_set" :label="t('Текущий пароль')" required>
          <UInput v-model="passwords.current_password" :aria-label="t('Текущий пароль')" name="current_password" type="password" autocomplete="current-password" :maxlength="1024" :disabled="changing || saving" required class="w-full" />
        </UFormField>
        <UFormField :label="t('Новый пароль')" required>
          <UInput v-model="passwords.new_password" :aria-label="t('Новый пароль')" aria-describedby="password-requirements" name="new_password" type="password" autocomplete="new-password" :maxlength="1024" :disabled="changing || saving" required class="w-full" />
        </UFormField>
        <UFormField :label="t('Подтвердите новый пароль')" required>
          <UInput v-model="passwords.confirm_password" :aria-label="t('Подтвердите новый пароль')" name="confirm_password" type="password" autocomplete="new-password" :maxlength="1024" :disabled="changing || saving" required class="w-full" />
        </UFormField>
        <p class="text-sm text-muted">{{ t('Все браузерные сессии будут завершены, включая текущую.') }}</p>
        <p v-if="passwordError" role="alert" class="text-sm text-error">{{ t(passwordError) }}</p>
        <UButton type="submit" :label="t(profile.password_set?'Изменить пароль':'Задать пароль')" icon="i-lucide-key-round" :loading="changing" :disabled="saving" />
      </form>
      <p v-else class="text-sm leading-relaxed text-muted">{{ t('Вход через Telegram. Парольный вход доступен только владельцу.') }}</p>
    </template>
  </section>
</template>
