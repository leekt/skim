// Keystore tests (no network): node wallet/test.mjs   (npm run -s wallet-test)
import assert from 'node:assert/strict'
import { generatePrivateKey, privateKeyToAddress } from 'viem/accounts'
import { decryptKeystore, encryptKeystore, keystoreFileName } from './keystore.mjs'

const tests = []
const test = (name, fn) => tests.push([name, fn])

// Test vectors from the Web3 Secret Storage Definition (ethereum/wiki, "Test Vectors"):
// password "testpassword", private key 7a28b5ba…fe9d. Only the MAC and ciphertext have to match,
// so a mistyped field fails the MAC check rather than passing silently.
const VECTOR_KEY = '0x7a28b5ba57c53603b0b07b56bba752f7784bf506fa95edc395f5cf6c7514fe9d'
const SCRYPT_VECTOR = {
  crypto: {
    cipher: 'aes-128-ctr',
    cipherparams: { iv: '83dbcc02d8ccb40e466191a123791e0e' },
    ciphertext: 'd172bf743a674da9cdad04534d56926ef8358534d458fffccd4e6ad2fbde479c',
    kdf: 'scrypt',
    kdfparams: { dklen: 32, n: 262144, r: 1, p: 8, salt: 'ab0c7876052600dd703518d6fc3fe8984592145b591fc8fb5c6d43190334ba19' },
    mac: '2103ac29920d71da29f15d75b4a16dbe95cfd7ff8faea1056c33131d846e3097',
  },
  id: '3198bc9c-6672-5ab3-d995-4942343ae5b6',
  version: 3,
}
const PBKDF2_VECTOR = {
  crypto: {
    cipher: 'aes-128-ctr',
    cipherparams: { iv: '6087dab2f9fdbbfaddc31a909735c1e6' },
    ciphertext: '5318b4d5bcd28de64ee5559e671353e16f075ecae9f99c7a79a38af5f869aa46',
    kdf: 'pbkdf2',
    kdfparams: { c: 262144, dklen: 32, prf: 'hmac-sha256', salt: 'ae3cd4e7013836a3df6bd7241b12db061dbe2c6785853cce422d148a624ce0bd' },
    mac: '517ead924a9d0dc3124507e3393d175ce3ff7c1e96529c6c555ce9e51205e9b2',
  },
  id: '3198bc9c-6672-5ab3-d995-4942343ae5b6',
  version: 3,
}

// Cheap scrypt parameters keep the round-trip tests fast; `new`/`import` use geth's standard ones.
const FAST = { n: 1024, r: 8, p: 1 }

test('encrypt -> decrypt round trip', () => {
  const key = generatePrivateKey()
  const ks = encryptKeystore(key, 'correct horse', FAST)
  assert.equal(ks.version, 3)
  assert.equal(ks.crypto.kdf, 'scrypt')
  assert.equal(ks.crypto.cipher, 'aes-128-ctr')
  assert.equal(`0x${ks.address}`, privateKeyToAddress(key).toLowerCase())
  assert.equal(decryptKeystore(JSON.stringify(ks), 'correct horse'), key)
})

test('round trip with the standard scrypt parameters (n=262144, r=8)', () => {
  const key = generatePrivateKey()
  assert.equal(decryptKeystore(encryptKeystore(key, 'pw'), 'pw'), key)
})

test('fresh salt and iv per encryption', () => {
  const key = generatePrivateKey()
  const a = encryptKeystore(key, 'pw', FAST), b = encryptKeystore(key, 'pw', FAST)
  assert.notEqual(a.crypto.kdfparams.salt, b.crypto.kdfparams.salt)
  assert.notEqual(a.crypto.cipherparams.iv, b.crypto.cipherparams.iv)
  assert.notEqual(a.crypto.ciphertext, b.crypto.ciphertext)
})

test('official scrypt test vector decrypts', () => {
  assert.equal(decryptKeystore(SCRYPT_VECTOR, 'testpassword'), VECTOR_KEY)
})

test('official pbkdf2 test vector decrypts', () => {
  assert.equal(decryptKeystore(PBKDF2_VECTOR, 'testpassword'), VECTOR_KEY)
})

test('wrong password is rejected by the MAC', () => {
  assert.throws(() => decryptKeystore(SCRYPT_VECTOR, 'testpassword!'), /MAC mismatch/)
  const ks = encryptKeystore(generatePrivateKey(), 'right', FAST)
  assert.throws(() => decryptKeystore(ks, 'wrong'), /MAC mismatch/)
})

test('tampered ciphertext is rejected by the MAC', () => {
  const ks = encryptKeystore(generatePrivateKey(), 'pw', FAST)
  const c = ks.crypto.ciphertext
  ks.crypto.ciphertext = (c[0] === '0' ? '1' : '0') + c.slice(1)
  assert.throws(() => decryptKeystore(ks, 'pw'), /MAC mismatch/)
})

test('geth-style file name', () => {
  const name = keystoreFileName('0xABCDEF0123456789abcdef0123456789ABCDEF01', new Date('2026-01-02T03:04:05.678Z'))
  assert.equal(name, 'UTC--2026-01-02T03-04-05.678Z--abcdef0123456789abcdef0123456789abcdef01')
})

let failed = 0
for (const [name, fn] of tests) {
  const t0 = performance.now()
  try {
    await fn()
    console.log(`ok    ${name}  (${(performance.now() - t0).toFixed(0)} ms)`)
  } catch (e) {
    failed++
    console.log(`FAIL  ${name}\n      ${e.stack ?? e}`)
  }
}
console.log(`${tests.length - failed}/${tests.length} passed`)
process.exit(failed ? 1 : 0)
