// Web3 Secret Storage v3 keystores (the JSON format geth, Foundry and MetaMask import/export),
// with Node's built-in crypto plus viem's keccak256. Demo code: not audited.
//
//   encrypt: derivedKey = scrypt(password, salt, n, r, p, dklen 32)
//            ciphertext = aes-128-ctr(key = derivedKey[0..16], iv, privateKey)
//            mac        = keccak256(derivedKey[16..32] ++ ciphertext)
//   decrypt: the same derivation; the MAC is checked before decrypting, so a wrong password is
//            rejected instead of producing a wrong key. pbkdf2 (hmac-sha256) keystores decrypt too.
import { createCipheriv, createDecipheriv, pbkdf2Sync, randomBytes, randomUUID, scryptSync, timingSafeEqual } from 'node:crypto'
import { bytesToHex, hexToBytes, keccak256 } from 'viem'
import { privateKeyToAddress } from 'viem/accounts'
// viem's own (pinned) dependency, used only for scrypt parameters OpenSSL refuses (see deriveKey).
import { scrypt as nobleScrypt } from '@noble/hashes/scrypt'

// geth's "standard" scrypt parameters: ~256 MiB and about a second per derivation, on purpose.
export const STANDARD = { n: 262144, r: 8, p: 1 }

const strip = (h) => (h.startsWith('0x') ? h.slice(2) : h)
const buf = (h) => Buffer.from(strip(h), 'hex')

function deriveKey(password, kdf, params) {
  const pw = Buffer.from(password.normalize('NFKC'), 'utf8')
  const salt = buf(params.salt)
  if (kdf === 'scrypt') {
    const { n, r, p, dklen } = params
    // OpenSSL enforces RFC 7914's N < 2^(16r), which some real keystores (including the
    // spec's own test vector: n=262144, r=1) break. Those go through the pure-JS scrypt.
    if (n < 2 ** (16 * r)) return scryptSync(pw, salt, dklen, { N: n, r, p, maxmem: 128 * (n + p + 2) * r + 64 * 1024 * 1024 })
    return Buffer.from(nobleScrypt(pw, salt, { N: n, r, p, dkLen: dklen, maxmem: 2 ** 31 }))
  }
  if (kdf === 'pbkdf2') {
    if (params.prf !== 'hmac-sha256') throw new Error(`unsupported pbkdf2 prf ${params.prf}`)
    return pbkdf2Sync(pw, salt, params.c, params.dklen, 'sha256')
  }
  throw new Error(`unsupported kdf ${kdf}`)
}

const mac = (derivedKey, ciphertext) =>
  hexToBytes(keccak256(Buffer.concat([derivedKey.subarray(16, 32), ciphertext])))

/** Encrypt a 0x-prefixed private key into a v3 keystore object. `kdfParams` overrides scrypt n/r/p (tests). */
export function encryptKeystore(privateKey, password, kdfParams = STANDARD) {
  const salt = randomBytes(32)
  const iv = randomBytes(16)
  const params = { dklen: 32, n: kdfParams.n, r: kdfParams.r, p: kdfParams.p, salt: salt.toString('hex') }
  const derivedKey = deriveKey(password, 'scrypt', params)
  const cipher = createCipheriv('aes-128-ctr', derivedKey.subarray(0, 16), iv)
  const ciphertext = Buffer.concat([cipher.update(buf(privateKey)), cipher.final()])
  return {
    address: strip(privateKeyToAddress(privateKey)).toLowerCase(),
    crypto: {
      cipher: 'aes-128-ctr',
      cipherparams: { iv: iv.toString('hex') },
      ciphertext: ciphertext.toString('hex'),
      kdf: 'scrypt',
      kdfparams: params,
      mac: Buffer.from(mac(derivedKey, ciphertext)).toString('hex'),
    },
    id: randomUUID(),
    version: 3,
  }
}

/** Decrypt a v3 keystore (object or JSON string). Returns the 0x-prefixed private key; throws on a wrong password. */
export function decryptKeystore(keystore, password) {
  const ks = typeof keystore === 'string' ? JSON.parse(keystore) : keystore
  if (ks.version !== 3) throw new Error(`unsupported keystore version ${ks.version}`)
  const c = ks.crypto ?? ks.Crypto
  if (c.cipher !== 'aes-128-ctr') throw new Error(`unsupported cipher ${c.cipher}`)
  const derivedKey = deriveKey(password, c.kdf, c.kdfparams)
  const ciphertext = buf(c.ciphertext)
  const expected = buf(c.mac)
  const actual = Buffer.from(mac(derivedKey, ciphertext))
  if (expected.length !== actual.length || !timingSafeEqual(expected, actual)) {
    throw new Error('wrong password (keystore MAC mismatch)')
  }
  const decipher = createDecipheriv('aes-128-ctr', derivedKey.subarray(0, 16), buf(c.cipherparams.iv))
  const key = Buffer.concat([decipher.update(ciphertext), decipher.final()])
  return bytesToHex(key)
}

/** geth's file name for a keystore: UTC--<ISO time with dashes>--<address>. */
export function keystoreFileName(address, date = new Date()) {
  return `UTC--${date.toISOString().replace(/:/g, '-')}--${strip(address).toLowerCase()}`
}
