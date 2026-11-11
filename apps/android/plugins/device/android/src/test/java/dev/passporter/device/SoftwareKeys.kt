package dev.passporter.device

import java.security.KeyPair
import java.security.KeyPairGenerator
import java.security.Signature
import java.security.interfaces.ECPublicKey
import java.security.spec.ECGenParameterSpec

/** [DeviceKeys] in memory, through the JDK's EC provider: DER signatures, like the Keystore's. */
internal class SoftwareKeys : DeviceKeys {
    val pairs = mutableMapOf<String, KeyPair>()
    private var next = 0

    override fun create(): NewKey {
        val pair = KeyPairGenerator.getInstance("EC").apply { initialize(ECGenParameterSpec("secp256r1")) }
            .generateKeyPair()
        val alias = "key-${next++}"
        pairs[alias] = pair
        return NewKey(alias, P256.sec1(pair.public as ECPublicKey))
    }

    override fun exists(alias: String) = alias in pairs

    override fun sign(alias: String, message: ByteArray): ByteArray {
        val pair = pairs[alias] ?: error("no key $alias")
        val der = Signature.getInstance("SHA256withECDSA").run {
            initSign(pair.private)
            update(message)
            sign()
        }
        return P256.derToP1363(der)
    }

    override fun delete(alias: String) {
        pairs.remove(alias)
    }

    /** Whether [signature] (`r || s`) over [message] verifies with [alias]'s public key. */
    fun verifies(alias: String, message: ByteArray, signature: ByteArray): Boolean =
        Signature.getInstance("SHA256withECDSAinP1363Format").run {
            initVerify(pairs.getValue(alias).public)
            update(message)
            verify(signature)
        }
}
