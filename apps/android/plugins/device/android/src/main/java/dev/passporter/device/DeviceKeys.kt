package dev.passporter.device

import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.PrivateKey
import java.security.ProviderException
import java.security.Signature
import java.security.interfaces.ECPublicKey
import java.security.spec.ECGenParameterSpec
import java.util.UUID

/** A key just made for pairing: its Keystore alias and its public point. */
internal class NewKey(val alias: String, val publicKey: ByteArray)

/** Where device keys live. [KeystoreKeys] on a phone; a software stand-in in tests. */
internal interface DeviceKeys {
    /** Makes a fresh P-256 signing key under a new alias. */
    fun create(): NewKey

    /** Whether the key [alias] still exists. */
    fun exists(alias: String): Boolean

    /** An ECDSA P-256 / SHA-256 signature over [message] as `r || s` (64 bytes). */
    fun sign(alias: String, message: ByteArray): ByteArray

    /** Deletes the key [alias], if it exists. */
    fun delete(alias: String)
}

/**
 * Device keys in the Android Keystore: non-exportable P-256 keys, in
 * StrongBox when the phone has one, otherwise in the TEE. They need no user
 * authentication, so a notification reply or the car can sign in while the
 * phone is locked (biometric step-up is a later phase).
 */
internal class KeystoreKeys : DeviceKeys {
    private fun keyStore(): KeyStore = KeyStore.getInstance(PROVIDER).apply { load(null) }

    override fun create(): NewKey {
        val alias = ALIAS_PREFIX + UUID.randomUUID()
        val publicKey = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            try {
                generate(alias, strongBox = true)
            } catch (e: ProviderException) {
                // StrongBoxUnavailableException, or a StrongBox that cannot
                // make this key: the TEE is the fallback.
                delete(alias)
                generate(alias, strongBox = false)
            }
        } else {
            generate(alias, strongBox = false)
        }
        return NewKey(alias, P256.sec1(publicKey))
    }

    private fun generate(alias: String, strongBox: Boolean): ECPublicKey {
        val spec = KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_SIGN)
            .setAlgorithmParameterSpec(ECGenParameterSpec("secp256r1"))
            .setDigests(KeyProperties.DIGEST_SHA256)
            .apply { if (strongBox && Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) setIsStrongBoxBacked(true) }
            .build()
        val generator = KeyPairGenerator.getInstance(KeyProperties.KEY_ALGORITHM_EC, PROVIDER)
        generator.initialize(spec)
        return generator.generateKeyPair().public as? ECPublicKey
            ?: throw ProviderException("the Keystore made a key that is not EC")
    }

    override fun exists(alias: String): Boolean = keyStore().containsAlias(alias)

    override fun sign(alias: String, message: ByteArray): ByteArray {
        val key = keyStore().getKey(alias, null) as? PrivateKey
            ?: throw ProviderException("no device key")
        val der = Signature.getInstance("SHA256withECDSA").run {
            initSign(key)
            update(message)
            sign()
        }
        return P256.derToP1363(der)
    }

    override fun delete(alias: String) {
        val store = keyStore()
        if (store.containsAlias(alias)) store.deleteEntry(alias)
    }

    private companion object {
        const val PROVIDER = "AndroidKeyStore"
        const val ALIAS_PREFIX = "passporter-device-"
    }
}
