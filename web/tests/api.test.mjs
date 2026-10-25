import {test} from 'node:test';
import assert from 'node:assert/strict';
import '../app/utils/i18n.ts';

test('nested app links keep API, login and attachment URLs under the configured prefix',async()=>{
  try {
    for(const prefix of ['/','/kodrik/','/nested/app/']) {
      for(const section of ['chat','pages','webhooks']) {
        globalThis.window={location:new URL(`https://example.com${prefix}app/${section}/telegram%3Amain%3A7`)};
        globalThis.document={getElementById:()=>({textContent:JSON.stringify({prefix})})};
        const {base,endpoint,login,attachmentUrl}=await import(`../app/utils/api.ts?${prefix}${section}`);
        assert.equal(base.href,`https://example.com${prefix}`);
        assert.equal(endpoint('sessions'),`https://example.com${prefix}api/chat/sessions`);
        const loginUrl=new URL(login());
        assert.equal(loginUrl.pathname,`${prefix}auth/login`);
        assert.equal(loginUrl.searchParams.get('return_to'),window.location.pathname);
        window.location=new URL(`https://example.com${prefix}app/pages/other`);
        assert.equal(new URL(login()).searchParams.get('return_to'),window.location.pathname);
        assert.equal(attachmentUrl('../api/chat/sessions/one/attachments/0/files/0'),`https://example.com${prefix}api/chat/sessions/one/attachments/0/files/0`);
        assert.equal(attachmentUrl(),undefined);
      }
    }
  } finally {delete globalThis.window;delete globalThis.document;}
});
