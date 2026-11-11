package dev.passporter.device

import java.io.File
import java.io.IOException
import java.nio.file.Files
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotSame
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

class DeviceAuthTest {
    private val dir: File = Files.createTempDirectory("device").toFile()
    private val keys = SoftwareKeys()
    private val bridge = FakeBridge()
    private var origin: BridgeOrigin? = BridgeOrigin.parse("https://bridge.example.org")
    private var clock = 1_000_000L
    private val store = DeviceStore(File(dir, "device.json"))
    private val auth = DeviceAuth({ origin }, store, keys, bridge, { clock })
    private val code = checkNotNull(PairingCode.parse("ABCD2345"))

    @After
    fun cleanUp() {
        dir.deleteRecursively()
    }

    /**
     * A bridge that pairs, issues challenges and verifies signatures with
     * the public key it was given, like `crates/bridge/src/auth`. Statuses
     * queued in [refuse] answer the next requests instead.
     */
    private inner class FakeBridge : HttpTransport {
        val sent = mutableListOf<HttpRequest>()
        val refuse = ArrayDeque<Int>()
        var publicKey: String? = null
        var challenge = 0
        var issued = 0
        var unreachable = false

        override fun send(request: HttpRequest): HttpResponse {
            sent += request
            if (unreachable) throw IOException("offline")
            refuse.removeFirstOrNull()?.let { return HttpResponse(it, "") }
            val body = JSONObject(request.body ?: "{}")
            return when (request.url) {
                "https://bridge.example.org/v1/auth/pair" -> {
                    publicKey = body.getString("publicKey")
                    HttpResponse(200, """{"deviceId":"dev-0123456789abcdef"}""")
                }
                "https://bridge.example.org/v1/auth/challenge" -> {
                    challenge++
                    HttpResponse(200, """{"challenge":"${"%064x".format(challenge)}","expiresInSecs":60}""")
                }
                "https://bridge.example.org/v1/auth/session" -> {
                    val alias = keys.pairs.entries.single { Hex.encode(P256.sec1(it.value.public as java.security.interfaces.ECPublicKey)) == publicKey }.key
                    val message = P256.signedMessage(
                        checkNotNull(DeviceId.parse(body.getString("deviceId"))),
                        checkNotNull(Challenge.parse(body.getString("challenge"))),
                    )
                    if (!keys.verifies(alias, message, Hex.decode(body.getString("signature")))) {
                        return HttpResponse(401, "")
                    }
                    issued++
                    val scope = body.optString("scope", "app")
                    HttpResponse(200, """{"token":"tok$issued","expiresInSecs":900,"deviceName":"Phone","scope":"$scope"}""")
                }
                else -> HttpResponse(if (request.headers["Authorization"] == "Bearer tok$issued") 200 else 401, "ok")
            }
        }
    }

    @Test
    fun pairsAndSignsIn() {
        assertFalse(auth.status().paired)
        val id = auth.pair(code, null)
        assertEquals("dev-0123456789abcdef", id.value)
        assertEquals(id, auth.status().deviceId)
        val token = auth.token(Scope.APP)
        assertEquals("tok1", token.value)
        assertEquals("Phone", token.deviceName)
        assertEquals("Bearer tok1", token.authorization())
        assertFalse(token.toString().contains("tok1"))
        assertEquals(900L, auth.secondsLeft(token))
        val pair = bridge.sent.first()
        assertEquals("POST", pair.method)
        assertEquals("application/json", pair.headers["Content-Type"])
    }

    @Test
    fun tokensAreCachedUntilShortlyBeforeTheyExpire() {
        auth.pair(code, null)
        val first = auth.token(Scope.APP)
        clock += (900 - 121) * 1000L
        assertSame(first, auth.token(Scope.APP))
        clock += 2000L
        val second = auth.token(Scope.APP)
        assertNotSame(first, second)
        assertEquals("tok2", second.value)
        // Scopes are cached apart.
        val car = auth.token(Scope.CAR)
        assertEquals(Scope.CAR, car.scope)
        assertSame(second, auth.token(Scope.APP))
        assertSame(car, auth.token(Scope.CAR))
        // A fresh token replaces the cached one.
        val fresh = auth.freshToken(Scope.APP)
        assertSame(fresh, auth.token(Scope.APP))
    }

    @Test
    fun invalidateDropsOnlyTheCurrentToken() {
        auth.pair(code, null)
        val old = auth.token(Scope.APP)
        val current = auth.freshToken(Scope.APP)
        auth.invalidate(old)
        assertSame(current, auth.token(Scope.APP))
        auth.invalidate(current)
        assertNotSame(current, auth.token(Scope.APP))
    }

    @Test
    fun aGoneDeviceIsForgotten() {
        auth.pair(code, null)
        val alias = keys.pairs.keys.single()
        bridge.refuse += 410
        assertThrows(DeviceAuthException.Forgotten::class.java) { auth.token(Scope.APP) }
        assertFalse(auth.status().paired)
        assertFalse(keys.exists(alias))
        assertThrows(DeviceAuthException.NotPaired::class.java) { auth.token(Scope.APP) }
    }

    @Test
    fun aRefusalIsRetriedOnceWithAFreshChallenge() {
        auth.pair(code, null)
        bridge.refuse += 401
        assertEquals("tok1", auth.token(Scope.APP).value)
        bridge.refuse += listOf(401, 401)
        assertThrows(DeviceAuthException.Refused::class.java) { auth.freshToken(Scope.APP) }
        assertTrue("a refusal keeps the pairing", auth.status().paired)
    }

    @Test
    fun otherFailuresAreTyped() {
        auth.pair(code, null)
        bridge.refuse += 429
        assertThrows(DeviceAuthException.RateLimited::class.java) { auth.token(Scope.APP) }
        bridge.refuse += 503
        assertThrows(DeviceAuthException.ServerError::class.java) { auth.token(Scope.APP) }
        bridge.refuse += 404
        assertThrows(DeviceAuthException.BadResponse::class.java) { auth.token(Scope.APP) }
        bridge.unreachable = true
        assertThrows(DeviceAuthException.Unreachable::class.java) { auth.token(Scope.APP) }
        assertTrue(auth.status().paired)
    }

    @Test
    fun aFailedPairingKeepsTheEarlierOne() {
        auth.pair(code, null)
        val alias = keys.pairs.keys.single()
        bridge.refuse += 401
        assertThrows(DeviceAuthException.Refused::class.java) { auth.pair(code, null) }
        assertEquals(setOf(alias), keys.pairs.keys)
        assertTrue(auth.status().paired)
        bridge.refuse += 409
        assertThrows(DeviceAuthException.TooManyDevices::class.java) { auth.pair(code, null) }
        assertEquals(setOf(alias), keys.pairs.keys)
    }

    @Test
    fun pairingAgainReplacesTheKeyAndTheTokens() {
        auth.pair(code, null)
        val old = keys.pairs.keys.single()
        val token = auth.token(Scope.APP)
        auth.pair(code, DeviceName.parse("Phone"))
        assertFalse(keys.exists(old))
        assertEquals(1, keys.pairs.size)
        assertEquals("Phone", JSONObject(checkNotNull(bridge.sent.last { it.url.endsWith("/pair") }.body)).getString("name"))
        assertNotSame(token, auth.token(Scope.APP))
    }

    @Test
    fun forgetDeletesEverything() {
        auth.pair(code, null)
        auth.token(Scope.APP)
        auth.forget()
        assertTrue(keys.pairs.isEmpty())
        assertNull(store.load())
        assertThrows(DeviceAuthException.NotPaired::class.java) { auth.token(Scope.APP) }
    }

    @Test
    fun listenersHearPairingAndForgetting() {
        val heard = mutableListOf<DeviceId?>()
        val listener = PairingListener { status ->
            // Outside the lock: a listener may call back in.
            assertEquals(status.deviceId, auth.status().deviceId)
            heard += status.deviceId
        }
        auth.addPairingListener(listener)
        val id = auth.pair(code, null)
        bridge.refuse += 409
        assertThrows(DeviceAuthException.TooManyDevices::class.java) { auth.pair(code, null) }
        auth.forget()
        auth.removePairingListener(listener)
        auth.pair(code, null)
        assertEquals(listOf(id, null), heard)
    }

    @Test
    fun aMissingKeyOrAnotherBridgeMeansUnpaired() {
        auth.pair(code, null)
        origin = BridgeOrigin.parse("https://other.example.org")
        assertFalse(auth.status().paired)
        origin = BridgeOrigin.parse("https://bridge.example.org")
        assertTrue(auth.status().paired)
        keys.pairs.clear()
        assertFalse(auth.status().paired)
        assertNull(store.load())
    }

    @Test
    fun noBridgeNoPairing() {
        origin = null
        assertThrows(DeviceAuthException.NoBridge::class.java) { auth.pair(code, null) }
        assertTrue(keys.pairs.isEmpty())
    }

    @Test
    fun aCorruptRecordIsDropped() {
        File(dir, "device.json").writeText("""{"deviceId":"nope"}""")
        assertNull(store.load())
        assertFalse(File(dir, "device.json").exists())
    }

    @Test
    fun sendRetriesA401OnceWithAFreshToken() {
        auth.pair(code, null)
        val stale = auth.token(Scope.APP)
        bridge.issued++ // the bridge restarted: tok1 is no longer valid
        val response = auth.send(Scope.APP, Method.POST, "/v1/agents/a/reply", """{"text":"hi"}""")
        assertEquals(200, response.status)
        val last = bridge.sent.last()
        assertEquals("https://bridge.example.org/v1/agents/a/reply", last.url)
        assertEquals("application/json", last.headers["Content-Type"])
        assertFalse("the request's toString leaves headers out", last.toString().contains("Bearer"))
        assertNotSame(stale, auth.token(Scope.APP))
        assertThrows(DeviceAuthException.InvalidInput::class.java) { auth.send(Scope.APP, Method.GET, "//evil.example/") }
        assertThrows(DeviceAuthException.InvalidInput::class.java) { auth.send(Scope.APP, Method.GET, "/v1/x", "{}") }
    }

    @Test
    fun aFailedSaveAfterPairingIsAStorageError() {
        dir.setWritable(false)
        try {
            assertThrows(DeviceAuthException.Storage::class.java) { auth.pair(code, null) }
            assertTrue("the unused key is deleted", keys.pairs.isEmpty())
        } finally {
            dir.setWritable(true)
        }
    }
}
