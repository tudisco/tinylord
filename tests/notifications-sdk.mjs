import assert from 'node:assert/strict';
import { TinyLord } from '../tinylord.js';
const calls = [];
const client = new TinyLord({baseUrl:'https://example.test', fetch:async (url,options)=>{
  calls.push({url,options});return new Response(JSON.stringify({id:'job-1',state:'pending'}),{status:200,headers:{'content-type':'application/json'}});
},readCookie:()=>null,clientId:'notification-test'});
client.accessToken='test-access-token';
const notifications=client.notifications();
await notifications.settings();
await notifications.configure({provider:'pushover',enabled:false});
await notifications.schedule({title:'Test',message:'Message',deliver_at:'2030-01-01T00:00:00Z'});
await notifications.status('task/id');
await notifications.cancel('task/id');
assert.deepEqual(calls.map(c=>[c.url,c.options.method]),[
 ['https://example.test/v1/notifications/settings','GET'],
 ['https://example.test/v1/notifications/settings','PUT'],
 ['https://example.test/v1/notifications','POST'],
 ['https://example.test/v1/notifications/task%2Fid','GET'],
 ['https://example.test/v1/notifications/task%2Fid','DELETE']
]);
assert.ok(calls.every(c=>c.options.headers.get('authorization')==='Bearer test-access-token'));
assert.equal(JSON.parse(calls[2].options.body).message,'Message');
console.log('PASS notification SDK routing, authentication, request bodies and escaped IDs');
