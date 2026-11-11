package dev.passporter.notify

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log
import org.json.JSONObject
import java.util.UUID

/**
 * Debug builds only: posts a sample notice message, through the same path
 * a decrypted push takes ([NoticeNotifier.show]), so a person can check
 * the notifications on a phone:
 *
 * ```sh
 * adb shell am broadcast -n dev.passporter.app/dev.passporter.notify.SampleNoticeReceiver --es sample blocked
 * ```
 *
 * [Sample] lists the samples. Each broadcast is a new message (a fresh
 * ping) unless `--es ping <hex>` names one: the same id twice is a retry of
 * the same message, which must not buzz.
 */
class SampleNoticeReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val name = intent.getStringExtra(EXTRA_SAMPLE)
        val sample = Sample.entries.firstOrNull { it.wire == name }
        if (sample == null) {
            val known = Sample.entries.joinToString { it.wire }
            Log.w(TAG, "unknown sample '$name'; one of: $known")
            return
        }
        val ping = intent.getStringExtra(EXTRA_PING) ?: UUID.randomUUID().toString().replace("-", "")
        val outcome = NoticeNotifier(context).show(message(ping, sample.notices))
        Log.i(TAG, "sample ${sample.wire}: $outcome")
    }

    /** The sample messages, by their `--es sample` name. */
    enum class Sample(
        /** The name to pass. */
        val wire: String,
        /** The notices, as JSON objects. */
        vararg val notices: String,
    ) {
        /** One agent at a dialog: alerts on "Needs you". */
        BLOCKED("blocked", BLOCKED_WIDGET),

        /** The same agent at a new dialog: alerts again, in place. */
        BLOCKED_AGAIN(
            "blocked-again",
            """{"agentId":"w1:p1","name":"widget","state":"blocked",
            "line":"Run `pnpm build`?","pendingId":"screen-3-cd","announce":true}""",
        ),

        /** A finished turn: quieter, on "Finished". */
        FINISHED("finished", FINISHED_DOCS),

        /** The same turn's Decide line: alerts once more. */
        DECIDE(
            "decide",
            """{"agentId":"w1:p2","name":"docs","state":"finished",
            "line":"Rewrote the install guide","decide":"Merge PR #12?","announce":true}""",
        ),

        /** Four agents at once, one conversation each. */
        MANY(
            "many",
            BLOCKED_WIDGET,
            FINISHED_DOCS,
            """{"agentId":"w2:p1","name":"bridge","state":"blocked"}""",
            """{"agentId":"w2:p2","name":"a very long agent name that goes on and on past the sixty character bound",
            "state":"finished","line":"${"word ".repeat(60)}"}""",
        ),
    }

    private companion object {
        const val TAG = "PassporterNotify"
        const val EXTRA_SAMPLE = "sample"
        const val EXTRA_PING = "ping"
    }
}

private const val BLOCKED_WIDGET =
    """{"agentId":"w1:p1","name":"widget","state":"blocked",
    "line":"Run `pnpm test`?","pendingId":"screen-2-ab","announce":true}"""

private const val FINISHED_DOCS =
    """{"agentId":"w1:p2","name":"docs","state":"finished","line":"Rewrote the install guide"}"""

private fun message(ping: String, notices: Array<out String>): String =
    """{"ping":${JSONObject.quote(ping)},"notices":[${notices.joinToString(",")}]}"""
