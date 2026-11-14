import {test} from 'node:test';
import assert from 'node:assert/strict';
import {parseChatMarkdown} from '../app/utils/chat-markdown.ts';

function nodes(node) { return [node,...(node.children || []).flatMap(nodes)]; }
function hasClass(node,name) { return [node.props?.class,node.props?.className].flat().filter(Boolean).join(' ').split(' ').includes(name); }

test('chat renders inline and block TeX as accessible KaTeX',async()=>{
  for(const [source,display] of [
    ['$x^2 + y_1$',false],
    ['$$\ny = f(w_1x_1 + w_2x_2 + \\dots + b)\n$$',true],
    [String.raw`\[ y = f(w_1x_1 + w_2x_2 + \dots + b) \]`,true],
    [String.raw`[ y = f(w_1x_1 + w_2x_2 + \dots + b) ]`,true],
    [String.raw`Inline \(x^2\) formula`,false],
  ]) {
    const all=nodes((await parseChatMarkdown(source)).body);
    assert.ok(all.some(node=>hasClass(node,'katex')),source);
    assert.equal(all.some(node=>hasClass(node,'katex-display')),display,source);
    assert.ok(all.some(node=>node.tag==='math'),source);
  }
});

test('chat math leaves code, links and ordinary brackets unchanged',async()=>{
  const source='`$x^2$` and `[ a_1 ]` and [ordinary text]\n\n[link](https://example.com)\n\n```tex\n\\[ x^2 \\]\n$$x$$\n```';
  const all=nodes((await parseChatMarkdown(source)).body);
  assert.equal(all.some(node=>hasClass(node,'katex')),false);
  assert.ok(all.some(node=>node.tag==='a' && node.props.href==='https://example.com'));
  assert.ok(all.some(node=>node.type==='text' && node.value.includes('\\[ x^2 \\]')));
});

test('chat sanitizes HTML and disallows trusted TeX commands without losing the message',async()=>{
  const parsed=await parseChatMarkdown(String.raw`<img src=x onerror="alert(1)"><script>alert(1)</script>

$\href{javascript:alert(1)}{click}$ and $\htmlClass{injected}{x}$ and $\unknowncommand{x}$

Still visible`);
  const all=nodes(parsed.body);
  assert.ok(all.some(node=>node.type==='text' && node.value.includes('Still visible')));
  assert.equal(all.some(node=>node.tag==='script' || node.props?.onerror || String(node.props?.href || '').startsWith('javascript:') || hasClass(node,'injected')),false);
});
