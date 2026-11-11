package dev.passporter.device

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Test

/** The wire shapes of `crates/protocol/src/auth.rs`. */
class ModelsTest {
    private val device = checkNotNull(DeviceId.parse("dev-0123456789abcdef"))
    private val challenge = checkNotNull(Challenge.parse("0f".repeat(32)))

    @Test
    fun valuesAreValidatedLikeTheProtocol() {
        assertNotNull(DeviceId.parse("dev-0123456789abcdef"))
        for (bad in listOf("0123456789abcdef", "dev-0123", "dev-0123456789ABCDEF", "dev-0123456789abcdeg")) {
            assertNull(bad, DeviceId.parse(bad))
        }
        assertNotNull(PairingCode.parse("ABCD2345"))
        for (bad in listOf("", "ABCD-2345", "abcd2345", "ABCDI345", "ABCDU345", "ABCD23456")) {
            assertNull(bad, PairingCode.parse(bad))
        }
        assertEquals("PairingCode(<redacted>)", PairingCode.parse("ABCD2345").toString())
        assertNotNull(DeviceName.parse("Pixel 8 Pro"))
        for (bad in listOf("", "  ", "a\u001b[2J", "a\nb", "a‮b", "x".repeat(49))) {
            assertNull(bad, DeviceName.parse(bad))
        }
        assertNull(Challenge.parse("0F".repeat(32)))
        assertNull(Challenge.parse("0f".repeat(31)))
        assertEquals(Scope.CAR, Scope.parse("car"))
        assertNull(Scope.parse("admin"))
    }

    @Test
    fun requestsUseTheProtocolsFieldNames() {
        val pair = JSONObject(
            PairRequest(checkNotNull(PairingCode.parse("ABCD2345")), byteArrayOf(4, 1, 2), null).toJson(),
        )
        assertEquals(setOf("code", "publicKey"), pair.keys().asSequence().toSet())
        assertEquals("040102", pair.getString("publicKey"))
        val named = JSONObject(
            PairRequest(checkNotNull(PairingCode.parse("ABCD2345")), byteArrayOf(4), DeviceName.parse("Phone")).toJson(),
        )
        assertEquals("Phone", named.getString("name"))

        assertEquals("""{"deviceId":"dev-0123456789abcdef"}""", ChallengeRequest(device).toJson())

        val app = JSONObject(SessionRequest(device, challenge, ByteArray(64) { 0xab.toByte() }, Scope.APP).toJson())
        assertEquals(setOf("deviceId", "challenge", "signature"), app.keys().asSequence().toSet())
        assertEquals("ab".repeat(64), app.getString("signature"))
        val car = JSONObject(SessionRequest(device, challenge, ByteArray(64), Scope.CAR).toJson())
        assertEquals("car", car.getString("scope"))
    }

    @Test
    fun answersAreParsedStrictly() {
        assertEquals(device, PairResponse.parse("""{"deviceId":"dev-0123456789abcdef"}""").deviceId)
        val c = ChallengeResponse.parse("""{"challenge":"${"0f".repeat(32)}","expiresInSecs":60}""")
        assertEquals(60L, c.expiresInSecs)

        val grant = SessionGrant.parse("""{"token":"abc.DEF-1","expiresInSecs":900,"deviceName":"Pixel","later":1}""")
        assertEquals("abc.DEF-1", grant.token)
        assertEquals(Scope.APP, grant.scope)
        assertEquals("Pixel", grant.deviceName)
        assertEquals(false, grant.toString().contains("abc.DEF-1"))
        assertEquals(Scope.CAR, SessionGrant.parse("""{"token":"t","expiresInSecs":900,"scope":"car"}""").scope)
        // Older bridges send no name.
        assertNull(SessionGrant.parse("""{"token":"t","expiresInSecs":900}""").deviceName)
        // A name this side would not show (a joiner, a control character) is dropped, not fatal.
        assertNull(SessionGrant.parse("""{"token":"t","expiresInSecs":900,"deviceName":"a\u200db"}""").deviceName)
        assertNull(SessionGrant.parse("""{"token":"t","expiresInSecs":900,"deviceName":"a\u001bb"}""").deviceName)

        val bad = listOf(
            """not json""",
            """{"deviceId":"dev-0123"}""",
            """{"deviceId":42}""",
        )
        for (body in bad) {
            assertThrows(body, DeviceAuthException.BadResponse::class.java) { PairResponse.parse(body) }
        }
        val badGrants = listOf(
            """{"token":"a b","expiresInSecs":900}""",
            """{"token":"t\r\nX: 1","expiresInSecs":900}""",
            """{"token":"t","expiresInSecs":"900"}""",
            """{"token":"t","expiresInSecs":-1}""",
            """{"token":"t","expiresInSecs":9.5}""",
            """{"token":"t","expiresInSecs":900,"scope":"admin"}""",
            """{"token":"t","expiresInSecs":900,"deviceName":7}""",
            """{"expiresInSecs":900}""",
        )
        for (body in badGrants) {
            assertThrows(body, DeviceAuthException.BadResponse::class.java) { SessionGrant.parse(body) }
        }
    }
}
