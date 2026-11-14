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
  const excluded = 'input,textarea,select,option,output,button,script,style,noscript,template,[contenteditable]:not([contenteditable="false"]),[role="textbox"],[role="searchbox"],[role="combobox"],[role="spinbutton"],[role="slider"],[role="listbox"]';
  let active = document.activeElement;
  while (active?.shadowRoot?.activeElement) active = active.shadowRoot.activeElement;
  const selected = window.getSelection();
  const selection = !active?.matches(excluded) && !!selected?.toString().trim();
  const ranges = selection ? Array.from({length:selected.rangeCount}, (_, i) => selected.getRangeAt(i)) : [];
  const root = document.querySelector('article, main, [role="main"]') || document.body;
  if (!root) throw new Error('У страницы нет доступного текста.');
  let length = 0, truncated = false;
  const visible = element => {
    // display:contents has no box itself, but its children can still be visible.
    const style = getComputedStyle(element);
    return !element.closest(excluded) && !element.hidden && style.fontSize !== '0px' &&
      (style.display === 'contents' ? style.visibility === 'visible' && Number(style.opacity) !== 0 :
        element.checkVisibility({checkOpacity:true,checkVisibilityCSS:true}));
  };
  const render = (node, pre = false) => {
    if (length > 100000) { truncated = true; return ''; }
    if (node.nodeType === Node.TEXT_NODE) {
      let text = node.textContent;
      if (selection) {
        text = ranges.map(range => {
          if (!range.intersectsNode(node)) return '';
          const start = range.startContainer === node ? range.startOffset : 0;
          const end = range.endContainer === node ? range.endOffset : text.length;
          return text.slice(start,end);
        }).join('');
      }
      text = pre ? text : text.replace(/\s+/g,' ');
      length += text.length;
      return text;
    }
    if (node.nodeType !== Node.ELEMENT_NODE || !visible(node)) return '';
    const tag = node.localName;
    if (tag === 'br') return '\n';
    if (tag === 'img') {
      if (selection && !ranges.some(range => range.intersectsNode(node))) return '';
      const alt = (node.getAttribute('alt') || '').replace(/\s+/g,' ');
      length += alt.length;
      return alt;
    }
    const children = tag === 'slot' ? (node.assignedNodes().length ? node.assignedNodes() : node.childNodes) :
      (node.shadowRoot ? node.shadowRoot.childNodes : node.childNodes);
    const text = Array.from(children, child => render(child, pre || tag === 'pre')).join('');
    if (!text.trim()) return text;
    if (selection) return /^(p|div|section|article|main|li|tr|h[1-6])$/.test(tag) ? `\n${text.trim()}\n` : text;
    if (/^h[1-6]$/.test(tag)) return `\n\n${'#'.repeat(Number(tag[1]))} ${text.trim()}\n\n`;
    if (tag === 'a') {
      try {
        const url = new URL(node.getAttribute('href'),document.baseURI);
        if (node.hasAttribute('href') && ['http:','https:'].includes(url.protocol) && !url.username && !url.password) {
          return `[${text.trim()}](${url.href.replace(/\(/g,'%28').replace(/\)/g,'%29')})`;
        }
      } catch {}
    }
    if (tag === 'pre') return `\n\n\`\`\`\n${text.replace(/\n$/,'')}\n\`\`\`\n\n`;
    if (tag === 'li') {
      const marker = node.parentElement?.localName === 'ol' ? `${Array.from(node.parentElement.children).indexOf(node) + 1}.` : '-';
      return `${marker} ${text.trim()}\n`;
    }
    if (tag === 'th' || tag === 'td') return `${text.trim().replace(/\|/g,'\\|').replace(/\n+/g,' ')} | `;
    if (tag === 'tr') return `| ${text.trim()}\n`;
    if (tag === 'table') {
      const rows = text.trim().split('\n').filter(row => row.trim());
      if (!rows.length) return '';
      const columns = rows[0].split(/(?<!\\)\|/).length - 2;
      rows.splice(1,0,`|${' --- |'.repeat(columns)}`);
      return `\n\n${rows.join('\n')}\n\n`;
    }
    if (tag === 'blockquote') return `\n\n> ${text.trim().replace(/\n/g,'\n> ')}\n\n`;
    if (['p','div','section','article','main','ul','ol','header','footer','aside','figure','figcaption'].includes(tag)) return `\n\n${text.trim()}\n\n`;
    return text;
  };
  // Walk the live composed tree: cloning loses visibility, selection and open shadow roots.
  const source = render(selection ? document.body : root).replace(/\n[ \t]+\n/g,'\n\n').replace(/\n{3,}/g,'\n\n').trim();
  let text = source.slice(0,100000);
  if (/[\uD800-\uDBFF]$/.test(text)) text = text.slice(0,-1);
  if (!text.trim()) throw new Error('Текст недоступен. Отключите контекст вкладки, чтобы отправить вопрос без него.');
  return {url:location.href,title:document.title,text,selection:!!selection,truncated:truncated || source.length > 100000};
}
