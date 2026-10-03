package com.promtuz.chat.ui.text

import org.junit.Assert.assertEquals
import org.junit.Test

class EmojiInputSyncTest {
    private sealed interface Step
    private data class Typed(val text: String) : Step
    private data class Ignored(val text: String) : Step
    private data class Applied(val text: String) : Step

    @Test fun `echoes never roll back typing and new drafts still apply`() {
        check("initial draft initializes native field once", "current draft",
            Applied("current draft"), Ignored("current draft"))
        check("delayed echoes cannot roll back newer typing", "",
            Typed("a"), Typed("ab"), Ignored("a"), Typed("abc"), Ignored("ab"), Ignored("abc"))
        check("coalesced echoes acknowledge skipped edits, so an older value restored later is a new draft", "",
            Typed("a"), Typed("ab"), Typed("abc"), Ignored("abc"), Applied("a"))
        check("send clears and replacement drafts still apply", "draft",
            Typed("draft 😀"), Applied(""), Applied("edit another message"),
            Typed("edit another message!"), Ignored("edit another message!"))
        check("unchanged initial value does not erase first keystrokes", "",
            Typed("a"), Ignored(""), Ignored("a"))
    }

    private fun check(case: String, initial: String, vararg steps: Step) {
        val sync = EmojiInputSync(initial)
        for ((i, step) in steps.withIndex()) when (step) {
            is Typed -> sync.publish(step.text)
            is Ignored -> assertEquals("$case: $step at $i", false, sync.acceptExternal(step.text))
            is Applied -> assertEquals("$case: $step at $i", true, sync.acceptExternal(step.text))
        }
    }
}
