import {test} from 'node:test';
import assert from 'node:assert/strict';
import {resolveLinkProps} from '../app/utils/link-target.ts';

test('embedded document links use the publication base without changing data or form values',()=>{
  const props={href:'../nutrition/',items:[{label:'Report',to:'../nutrition/',children:[{href:'#section'},{href:'https://other.example/report'}]}],avatar:{src:'./photo.png'},data:[{href:'raw value'}],state:{to:'Alice'},modelValue:{href:'saved value'},value:{to:'saved destination'}};
  const resolved=resolveLinkProps(props,'https://example.com/kodrik/pages/report/');
  assert.deepEqual(resolved,{
    href:'https://example.com/kodrik/pages/nutrition/',
    items:[{label:'Report',to:'https://example.com/kodrik/pages/nutrition/',children:[{href:'#section'},{href:'https://other.example/report'}]}],
    avatar:{src:'https://example.com/kodrik/pages/report/photo.png'},
    data:[{href:'raw value'}],state:{to:'Alice'},modelValue:{href:'saved value'},value:{to:'saved destination'},
  });
  assert.equal(props.href,'../nutrition/');
  assert.equal(props.items[0].to,'../nutrition/');
  assert.equal(props.avatar.src,'./photo.png');
});
