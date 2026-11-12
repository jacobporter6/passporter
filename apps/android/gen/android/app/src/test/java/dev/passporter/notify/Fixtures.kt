package dev.passporter.notify

import dev.passporter.device.DeviceId

/** A valid agent id; fails the test on a typo. */
fun agent(raw: String): AgentId = requireNotNull(AgentId.parse(raw)) { raw }

/** A notice of [agent], blocked at dialog [pending] unless changed. */
fun notice(
    agent: String = "w1:p1",
    name: String = "widget",
    state: NoticeState = NoticeState.BLOCKED,
    line: String? = "Run `pnpm test`?",
    decide: String? = null,
    pending: String? = "screen-2-ab",
    announce: Boolean = true,
): Notice =
    Notice(
        agentId = agent(agent),
        name = name,
        state = state,
        line = line,
        decide = decide,
        pendingId = pending?.let { requireNotNull(PendingId.parse(it)) },
        announce = announce,
    )

/** A message of [notices]. */
fun messageOf(vararg notices: Notice, ping: String = "ab"): NoticeMessage =
    NoticeMessage(ping, notices.toList())

/** The device the app is paired as in the tests, and the notices came to. */
val DEVICE: DeviceId = requireNotNull(DeviceId.parse("dev-0123456789abcdef"))

/** Another device: an older pairing's, or another bridge's. */
val OTHER_DEVICE: DeviceId = requireNotNull(DeviceId.parse("dev-fedcba9876543210"))
