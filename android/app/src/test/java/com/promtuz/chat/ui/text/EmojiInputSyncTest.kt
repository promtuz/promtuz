package com.promtuz.chat.ui.text

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class EmojiInputSyncTest {
    @Test
    fun `initial draft initializes native field once`() {
        val sync = EmojiInputSync("current draft")
        assertTrue(sync.acceptExternal("current draft"))
        assertFalse(sync.acceptExternal("current draft"))
    }

    @Test
    fun `delayed echoes cannot roll back newer typing`() {
        val sync = EmojiInputSync("")
        sync.publish("a")
        sync.publish("ab")
        assertFalse(sync.acceptExternal("a"))
        sync.publish("abc")
        assertFalse(sync.acceptExternal("ab"))
        assertFalse(sync.acceptExternal("abc"))
    }

    @Test
    fun `coalesced echoes acknowledge skipped edits`() {
        val sync = EmojiInputSync("")
        sync.publish("a")
        sync.publish("ab")
        sync.publish("abc")
        assertFalse(sync.acceptExternal("abc"))
        // An older value deliberately restored later is now a programmatic edit.
        assertTrue(sync.acceptExternal("a"))
    }

    @Test
    fun `send clears and replacement drafts still apply`() {
        val sync = EmojiInputSync("draft")
        sync.publish("draft 😀")
        assertTrue(sync.acceptExternal(""))
        assertTrue(sync.acceptExternal("edit another message"))
        sync.publish("edit another message!")
        assertFalse(sync.acceptExternal("edit another message!"))
    }

    @Test
    fun `unchanged initial value does not erase first keystrokes`() {
        val sync = EmojiInputSync("")
        sync.publish("a")
        assertFalse(sync.acceptExternal(""))
        assertFalse(sync.acceptExternal("a"))
    }
}
