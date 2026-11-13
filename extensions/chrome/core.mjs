export function serverURL(value) {
  const url = new URL(value.trim());
  if ((url.protocol !== 'https:' && !(url.protocol === 'http:' && ['localhost', '127.0.0.1'].includes(url.hostname))) || url.username || url.password || url.search || url.hash) {
    throw new Error('Укажите HTTPS-адрес Кодрика без пароля, параметров и # (HTTP разрешён только для localhost).');
  }
  if (!url.pathname.endsWith('/')) url.pathname += '/';
  return url;
}

export function truncateUTF8(text, budget) {
  const bytes = new TextEncoder().encode(text);
  if (bytes.length <= budget) return text;
  let end = budget;
  while (end > 0 && (bytes[end] & 0xc0) === 0x80) end--;
  return new TextDecoder().decode(bytes.subarray(0, end));
}

export function pageContext(page) {
  const url = new URL(page.url);
  if (!['https:', 'http:'].includes(url.protocol) || url.username || url.password || url.href.length>4096) throw new Error('Эту вкладку прочитать нельзя.');
  const context={url:url.href,title:String(page.title).slice(0,1024),text:'',source:page.selection?'selection':'page',captured_at:new Date().toISOString(),truncated:!!page.truncated};
  const text=String(page.text);
  context.text=text;
  if(new TextEncoder().encode(JSON.stringify(context)).length>100000) {
    context.truncated=true;
    let low=0,high=text.length;
    while(low<high) {
      const mid=Math.ceil((low+high)/2);
      context.text=text.slice(0,mid);
      if(new TextEncoder().encode(JSON.stringify(context)).length<=100000) low=mid;else high=mid-1;
    }
    // Never leave half a UTF-16 surrogate pair at the truncation boundary.
    if(low && /[\uD800-\uDBFF]/.test(text[low-1])) low--;
    context.text=text.slice(0,low);
  }
  return context;
}

// Executed in the isolated world, only in response to a Send request.
export function extractPage() {
  if (document.contentType !== 'text/html' && document.contentType !== 'application/xhtml+xml' && document.contentType !== 'text/plain') throw new Error('Поддерживаются только текстовые страницы, не PDF или видео.');
  const active = document.activeElement;
  const editing = active?.matches('input, textarea, [contenteditable]:not([contenteditable="false"])');
  const selection = editing ? '' : window.getSelection()?.toString().trim();
  const root = document.querySelector('article, main, [role="main"]') || document.body;
  if (!root) throw new Error('У страницы нет доступного текста.');
  // innerText respects visibility; clone text never includes form values or editable fields.
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  const parts = [];
  let length = 0, truncated = false, node;
  if (!selection) while ((node = walker.nextNode())) {
    const element = node.parentElement;
    if (!element || element.closest('input,textarea,select,script,style,noscript,[contenteditable]:not([contenteditable="false"])') || !element.checkVisibility({checkOpacity:true,checkVisibilityCSS:true})) continue;
    const text = node.textContent.trim();
    if (!text) continue;
    parts.push(text);
    length += text.length + 1;
    if (length > 100000) { truncated = true; break; }
  }
  const text = selection ? selection.slice(0, 100000) : parts.join('\n');
  if (!text.trim()) throw new Error('Текст недоступен. Отключите контекст вкладки, чтобы отправить вопрос без него.');
  return {url:location.href,title:document.title,text,selection:!!selection,truncated:truncated || (selection?.length || 0) > 100000};
}
