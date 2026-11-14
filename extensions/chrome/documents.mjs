// Self-contained: Chrome serializes this function into the owning tab's isolated world.
export async function readDocument(expectedUrl, preview = false) {
  if (location.href !== expectedUrl) throw new Error('Вкладка изменилась во время чтения.');
  const url = new URL(expectedUrl);
  if (!['https:', 'http:'].includes(url.protocol) || url.username || url.password) throw new Error('Недоступный адрес документа.');
  const match = url.hostname === 'docs.google.com' && url.pathname.match(/^\/(document|presentation|spreadsheets)\/d\/([^/]+)\//);
  const pdf = document.contentType === 'application/pdf';
  if (!pdf && !match) return null;
  const format = pdf || match[1] === 'presentation' ? 'pdf' : match[1] === 'spreadsheets' ? 'xlsx' : 'md';
  const mime = format === 'pdf' ? 'application/pdf' : format === 'xlsx' ? 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet' : 'text/markdown';
  const name = `${(document.title || 'document').replace(/[\\/\x00-\x1f]/g, '_').slice(0, 160)}.${format}`;
  const metadata = {url:expectedUrl,title:document.title,name,mime};
  if (preview) return metadata;
  const target = pdf ? url : new URL(`https://docs.google.com/${match[1]}/d/${match[2]}/export`);
  if (!pdf) {
    target.searchParams.set('format', format);
    if (url.searchParams.has('tab')) target.searchParams.set('tab', url.searchParams.get('tab'));
  }
  // Match the server's existing attachment budget, not ChatGPT's 128 MiB limit.
  const limit = 20_000_000;
  const response = await fetch(target.href, {credentials:'include',redirect:'error',signal:AbortSignal.timeout(60000)});
  if (!response.ok || !response.body) throw new Error('Не удалось скачать документ. Проверьте доступ и разрешение на экспорт.');
  const contentType = response.headers.get('content-type')?.split(';')[0].trim().toLowerCase();
  if ((format === 'pdf' && contentType !== mime) || contentType === 'text/html') {await response.body.cancel();throw new Error('Вместо документа сервер вернул страницу входа или другой формат.');}
  if (Number(response.headers.get('content-length')) > limit) {await response.body.cancel();throw new Error('Документ превышает 20 MB.');}
  const reader = response.body.getReader(), chunks = [];
  let size = 0;
  try {
    for (;;) {
      const {done,value} = await reader.read();
      if (done) break;
      size += value.length;
      if (size > limit) throw new Error('Документ превышает 20 MB.');
      chunks.push(value);
    }
  } finally {await reader.cancel();}
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {bytes.set(chunk, offset);offset += chunk.length;}
  if (!size || format === 'pdf' && new TextDecoder().decode(bytes.subarray(0,5)) !== '%PDF-' || format === 'xlsx' && (bytes[0] !== 80 || bytes[1] !== 75)) throw new Error('Сервер вернул некорректный документ.');
  if (location.href !== expectedUrl) throw new Error('Вкладка изменилась во время чтения.');
  let binary = '';
  for (let i=0;i<bytes.length;i+=32768) binary += String.fromCharCode(...bytes.subarray(i,i+32768));
  return {...metadata,base64:btoa(binary)};
}
