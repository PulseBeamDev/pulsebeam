import assert from 'node:assert/strict';
import { createPublicKey, verify } from 'node:crypto';
import { readFileSync } from 'node:fs';
const vectors = JSON.parse(readFileSync(new URL('./vectors.json', import.meta.url)));
assert.equal(vectors.version, 1);
for (const v of vectors.valid) {
  assert.equal(v.public_key_hex, '3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c');
  assert.equal(Buffer.from(v.header).toString('base64url') + '.' + Buffer.from(v.claims).toString('base64url'), v.signing_input);
  assert.equal(v.token, v.signing_input + '.' + v.signature);
  const publicKey = createPublicKey({key: Buffer.concat([Buffer.from('302a300506032b6570032100', 'hex'), Buffer.from(v.public_key_hex, 'hex')]), format: 'der', type: 'spki'});
  assert(verify(null, Buffer.from(v.signing_input), publicKey, Buffer.from(v.signature, 'base64url')), v.name);
}
