import {setLanguage} from '../app/utils/i18n.ts';
setLanguage('ru');
import {test} from 'node:test';
import assert from 'node:assert/strict';
import {createChartScene,renderChartSvg,resolveFocusPresentation} from '@tanstack/charts';
import * as adapter from '../app/utils/ui-chart.ts';

const props={data:[{day:'A',kcal:1500},{day:'B',kcal:2500}],index:'day',categories:[{key:'kcal',label:'Калории',color:'primary'}],yDomain:[1001,3499]};
const scene=props=>createChartScene(adapter.prepareScaledChart(props).definition,{width:640,height:300});

test('fixed Y domain stays exact across data changes and never mutates rows',()=>{
  assert.equal(typeof adapter.prepareScaledChart,'function');
  const before=structuredClone(props);
  for(const data of [props.data,[{day:'A',kcal:2000}],[{day:'A',kcal:-500},{day:'B',kcal:5000}],[],[{day:'A',kcal:null}]]) {
    const result=scene({...props,data});
    assert.deepEqual(result.scales.y.domain,[1001,3499]);
    assert.doesNotMatch(renderChartSvg(result,{ariaLabel:'Fixed'}),/NaN|Infinity/);
  }
  assert.deepEqual(props,before);
  assert.equal(adapter.prepareScaledChart({...props,yDomain:undefined}),undefined);
});

test('out-of-domain values are clipped rather than clamped or dropped',()=>{
  assert.equal(typeof adapter.prepareScaledChart,'function');
  const result=scene({...props,data:[{day:'A',kcal:500},{day:'B',kcal:2000},{day:'C',kcal:5000}]});
  assert.deepEqual(result.points.map(point=>point.datum.value),[500,2000,5000]);
  assert.ok(result.points[0].y>result.chart.y+result.chart.height);
  assert.ok(result.points[2].y<result.chart.y);
  assert.match(renderChartSvg(result,{ariaLabel:'Clipped'}),/clip-path=/);
});

test('fixed domain preserves curves, null gaps, guides and stacked Cartesian modes',()=>{
  assert.equal(typeof adapter.prepareScaledChart,'function');
  for(const type of ['line','area','bar']) for(const stacked of [false,true]) {
    const result=scene({...props,type,stacked,data:[{day:'A',kcal:1500,extra:100},{day:'B',kcal:2000,extra:200}],categories:['kcal','extra']});
    assert.deepEqual(result.scales.y.domain,[1001,3499]);
    assert.ok(result.points.some(point=>point.datum.top===(stacked?2200:2000)));
  }
  for(const curve of ['monotone','step']) {
    const svg=renderChartSvg(scene({...props,curve,data:[{day:'A',kcal:1500},{day:'B',kcal:2400},{day:'C',kcal:1800}]}),{ariaLabel:curve});
    assert.match(svg,curve==='monotone'?/C[\d.-]+,/:/L[\d.-]+,/);
  }
  const gap=scene({...props,points:true,data:[props.data[0],{day:'gap',kcal:null},props.data[1]],xAxis:false,yAxis:false,grid:false,legend:false});
  assert.equal(gap.points.length,2);
  assert.doesNotMatch(renderChartSvg(gap,{ariaLabel:'Gap'}),/NaN|Infinity/);
});

test('domain validation rejects malformed tuples and incompatible chart modes',()=>{
  assert.equal(typeof adapter.checkChartYDomain,'function');
  for(const yDomain of [null,{},'0,10',[],[0],[0,1,2],['0',10],[0,null],[NaN,10],[0,Infinity],[10,10],[10,0],[-Number.MAX_VALUE,Number.MAX_VALUE],[0,Number.MIN_VALUE]]) {
    assert.throws(()=>adapter.checkChartYDomain({...props,yDomain}));
  }
  for(const extra of [{type:'donut'},{definition:{}},{type:'unknown'}]) assert.throws(()=>adapter.checkChartYDomain({...props,...extra}));
  assert.deepEqual(adapter.checkChartYDomain({...props,yDomain:[-20.5,0]}),[-20.5,0]);
  assert.equal(adapter.checkChartYDomain({type:'donut'}),undefined);
});

test('fixed domain clips keyboard focus rings as well as marks',()=>{
  const result=scene({...props,data:[{day:'A',kcal:990}]});
  const presentation=resolveFocusPresentation(result,{primary:result.points[0],group:result.points,source:'keyboard',pinned:false});
  const focus=presentation.over.find(node=>node.key.startsWith('default-focus:'));
  assert.deepEqual(focus.clip,result.chart);
});

test('scaled tooltip scope preserves category order, labels and unstacked values',()=>{
  const chart=adapter.prepareScaledChart({...props,type:'area',stacked:true,categories:[{key:'kcal',label:'Калории'},{key:'extra',label:'Добавка'}],data:[{day:'A',kcal:1500,extra:400}]});
  const result=createChartScene(chart.definition,{width:640,height:300});
  assert.equal(typeof chart.tooltipScope,'function');
  const scope=chart.tooltipScope({title:'A',points:[...result.points].reverse(),primaryPoint:result.points[1],rows:[]});
  assert.deepEqual(scope.rows.map(row=>[row.label,row.value,row.active]),[['Калории',chart.format(1500),false],['Добавка',chart.format(400),true]]);
});
