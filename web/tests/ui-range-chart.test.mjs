import {setLanguage} from '../app/utils/i18n.ts';
setLanguage('ru');
import {test} from 'node:test';
import assert from 'node:assert/strict';
import {createChartScene,renderChartSvg} from '@tanstack/charts';
import {prepareRangeChart} from '../app/utils/ui-range-chart.ts';

const data=[{date:'A',low:20,high:40,target:45},{date:'B',low:40,high:80,target:45},{date:'C',low:30,high:50,target:45}];
const props={data,index:'date',range:{min:'low',max:'high'},categories:[{key:'target',label:'Ориентир'}]};

test('range chart paints a translucent band and midpoint without boundary lines',()=>{
  const chart=prepareRangeChart(props);
  assert.deepEqual(chart.rows.map(row=>row.mid),[30,60,40]);
  const scene=createChartScene(chart.definition,{width:640,height:300});
  const svg=renderChartSvg(scene,{ariaLabel:'Test range'});
  assert.match(svg,/fill-opacity="0.2"/);
  assert.doesNotMatch(svg,/NaN|Infinity|linearGradient/);
  assert.deepEqual(chart.categories.map(category=>category.label),['Среднее','Ориентир']);
  assert.deepEqual(scene.points.filter(point=>point.datum.category==='__range_midpoint').map(point=>point.datum.value),[30,60,40]);
  assert.equal(scene.points.length,6);
  const flatten=nodes=>nodes.flatMap(node=>[node,...flatten(node.children??[])]);
  const nodes=flatten(scene.nodes),band=nodes.find(node=>node.kind==='area'),middle=nodes.find(node=>node.kind==='polyline'&&node.key.includes('__range_midpoint'));
  assert.equal(nodes.filter(node=>node.kind==='area').length,1);
  assert.equal(nodes.filter(node=>node.kind==='polyline'&&node.key.startsWith('range-lines:')).length,2);
  middle.points.forEach((point,index)=>assert.ok(Math.abs(point[1]-(band.points[index][1]+band.points[5-index][1])/2)<.001));
  assert.equal(data[0].low,20);
  const existing=prepareRangeChart({...props,categories:['low','high','target']});
  assert.equal(existing.categories.length,2);
});

test('range chart preserves null gaps, equal bounds and finite extremes',()=>{
  const cases=[{date:'A',low:null,high:40},{date:'B',low:20,high:null},{date:'C',low:40,high:40},{date:'D',low:Number.MAX_VALUE,high:Number.MAX_VALUE}];
  const chart=prepareRangeChart({...props,categories:[],data:cases});
  assert.deepEqual(chart.rows.map(row=>row.mid),[undefined,undefined,40,Number.MAX_VALUE]);
  const gaps=prepareRangeChart({...props,categories:[],data:[data[0],{date:'B',low:null,high:null},data[2]]});
  const svg=renderChartSvg(createChartScene(gaps.definition,{width:640,height:300}),{ariaLabel:'Gaps'});
  assert.equal((svg.match(/<circle[^>]+data-ts-key="range-lines:[^"]+:dot"/g)??[]).length,2,'isolated observations remain visible across null gaps');
  assert.equal((svg.match(/data-ts-key="range-band:/g)??[]).length,2,'the band does not bridge null gaps');
  for(const rows of [[],[data[0]],[{date:'A',low:40,high:40}]]) {
    const result=prepareRangeChart({...props,categories:[],data:rows});
    assert.doesNotMatch(renderChartSvg(createChartScene(result.definition,{width:640,height:300}),{ariaLabel:'Test range'}),/NaN|Infinity/);
  }
});

test('range chart rejects malformed bounds and incompatible modes',()=>{
  for(const extra of [{data:[{date:'A',low:50,high:10,target:45}]},{data:[{date:'A',low:'20',high:40,target:45}]},{data:[{date:'A',low:20,target:45}]},{data:[data[0],data[0]]},{range:{min:'low',max:'high',unknown:1}},{type:'bar'},{curve:'monotone'},{stacked:true},{definition:{}},{categories:['missing']}]) {
    assert.throws(()=>prepareRangeChart({...props,...extra}));
  }
});
