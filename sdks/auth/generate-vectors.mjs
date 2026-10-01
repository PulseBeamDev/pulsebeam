// Maintenance only: regenerate with Node.js, never from an SDK implementation.
import { createPrivateKey, createPublicKey, sign } from 'node:crypto';
import { writeFileSync } from 'node:fs';

const alphabet = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
function base32(bytes, highPadding) {
  let n = BigInt('0x' + bytes.toString('hex'));
  const count = Math.ceil(bytes.length * 8 / 5);
  if (!highPadding) n <<= BigInt(count * 5 - bytes.length * 8);
  let result = '';
  for (let i = 0; i < count; i++) {
    result = alphabet[Number(n & 31n)] + result;
    n >>= 5n;
  }
  return result;
}
const id = (prefix, hex) => prefix + '_0' + base32(Buffer.from(hex, 'hex'), false);
const project = id('p', '018f4f7c200070008000000000000031');
const key = id('kid', '018f4f7c200170018001000000000032');
// RFC 8032 section 7.1 test 2, distinct from PulseBeam development credentials.
const seed = Buffer.from('4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb', 'hex');
const secret = 'sk_0' + base32(seed, true);
const privateKey = createPrivateKey({key: Buffer.concat([Buffer.from('302e020100300506032b657004220420', 'hex'), seed]), format: 'der', type: 'pkcs8'});
const publicKey = createPublicKey(privateKey).export({format: 'der', type: 'spki'}).subarray(-32);
const base = {project_id: project, key_id: key, secret, room: 'Room_9-Z', participant: 'Alice_-9', expiration: '2000'};
const cases = [
  ['registered', {}],
  ['expired-zero', {expiration: '0', room: '-', participant: '_'}],
  ['safe-number-boundary', {expiration: '9007199254740991'}],
  ['beyond-safe-number', {expiration: '9007199254740993'}],
  ['u64-max', {expiration: '18446744073709551615', room: 'X'.repeat(36), participant: 'z'.repeat(36)}],
  ['aliases', {project_id: project.replaceAll('0', 'o').replaceAll('1', 'l').toLowerCase(), key_id: key.replaceAll('0', 'O').replaceAll('1', 'I'), secret: secret.replaceAll('0', 'o').replaceAll('1', 'l').toLowerCase()}],
];
const valid = cases.map(([name, overrides]) => {
  const input = {...base, ...overrides};
  const header = JSON.stringify({alg: 'EdDSA', kid: key, typ: 'pb+jwt'});
  const claims = `{"iss":"${project}","aud":"pb","sub":"${input.participant}","room":"${input.room}","exp":${input.expiration}}`;
  const signing_input = Buffer.from(header).toString('base64url') + '.' + Buffer.from(claims).toString('base64url');
  const signature = sign(null, Buffer.from(signing_input), privateKey).toString('base64url');
  return {name, input, canonical_project_id: project, canonical_key_id: key, seed_hex: seed.toString('hex'), public_key_hex: publicKey.toString('hex'), header, claims, signing_input, signature, token: signing_input + '.' + signature};
});
const invalid = [];
function reject(field, name, value) { invalid.push({name: field + '-' + name, field, value}); }
for (const field of ['project_id', 'key_id', 'secret', 'room', 'participant', 'expiration']) {
  reject(field, 'missing', null);
  reject(field, 'boolean', true);
  if (field !== 'expiration') { reject(field, 'empty', ''); reject(field, 'number', 42); }
}
for (const field of ['room', 'participant']) {
  for (const [name, value] of Object.entries({long: 'x'.repeat(37), space: 'has space', slash: 'a/b', unicode: 'café', newline: 'abc\n', leading: ' abc'})) reject(field, name, value);
}
for (const [field, prefix, value] of [['project_id', 'p', project], ['key_id', 'kid', key]]) {
  const uuid = Buffer.from('018f4f7c200070008000000000000031', 'hex');
  const badVersion = Buffer.from(uuid); badVersion[6] = 0x40;
  const badVariant = Buffer.from(uuid); badVariant[8] = 0;
  for (const [name, bad] of Object.entries({prefix: value.toUpperCase(), wrong_prefix: value.replace(prefix + '_', 'rm_'), version: value.replace('_0', '_1'), short: value.slice(0, -1), long: value + '0', character: value.slice(0, -1) + 'U', unicode: value.slice(0, prefix.length + 2) + value.slice(prefix.length + 2).replace(/[A-Z]/, 'ſ'), padding: value.slice(0, -1) + '1', whitespace: ' ' + value, newline: value + '\n', uuid_version: id(prefix, badVersion.toString('hex')), uuid_variant: id(prefix, badVariant.toString('hex'))})) reject(field, name, bad);
}
for (const [name, value] of Object.entries({prefix: secret.replace('sk_', 'SK_'), public_key: 'pk_0' + base32(publicKey, true), version: secret.replace('_0', '_1'), short: secret.slice(0, -1), long: secret + '0', expanded: 'sk_0' + base32(Buffer.concat([seed, publicKey]), true), character: secret.slice(0, -1) + 'U', unicode: secret.replace('S', 'ſ'), padding: 'sk_02' + secret.slice(5), whitespace: ' ' + secret, newline: secret + '\n'})) reject('secret', name, value);
for (const [name, value] of Object.entries({negative: '-1', fractional: '1.5', overflow: '18446744073709551616', lossy_number: '9007199254740993', string: '2000', nan: 'NaN', infinity: 'Infinity'})) {
  // Distinguish an inexact native number from the exact decimal adapter used for valid fixtures.
  invalid.push({name: 'expiration-' + name, field: 'expiration', value, representation: name === 'string' ? 'string' : 'number'});
}
writeFileSync(new URL('./vectors.json', import.meta.url), JSON.stringify({version: 1, base, valid, invalid}, null, 2) + '\n');
