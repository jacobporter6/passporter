package dev.passporter.notify

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
fun messageOf(vararg notices: Notice): NoticeMessage = NoticeMessage("ab", notices.toList())
