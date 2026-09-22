package com.promtuz.chat.ui.text

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class EmojiSequencesTest {
    private val pack = setOf(
        "1f600", "2764", "1f469_200d_1f4bb", "1f44b_1f3fd", "1f1fa_1f1f8", "0023_20e3", "00a9",
        "1f1f3_1f1f4",
    )
    private val aliases = mapOf("1f1e7_1f1fb" to "1f1f3_1f1f4")

    private fun split(s: String) = EmojiSequences.split(s, pack, aliases)
    private fun emoji(cluster: String, key: String) = EmojiRun.Emoji(cluster, key)

    @Test
    fun `plain text around an emoji becomes three runs`() {
        assertEquals(
            listOf(EmojiRun.Text("hi "), emoji("😀", "1f600"), EmojiRun.Text(" there")),
            split("hi 😀 there"),
        )
    }

    @Test
    fun `a variation selector selects the picture and is dropped from the key`() {
        assertEquals(listOf(emoji("❤️", "2764")), split("❤️"))
        assertEquals("bare heart is text presentation", listOf(EmojiRun.Text("❤")), split("❤"))
    }

    @Test
    fun `a symbol the pack happens to hold stays text unless asked for as emoji`() {
        assertEquals(listOf(EmojiRun.Text("© 2026")), split("© 2026"))
        assertEquals(listOf(emoji("©️", "00a9")), split("©️"))
    }

    @Test
    fun `joined sequences, skin tones, flags and keycaps are one cluster each`() {
        val family = "👩‍💻" // woman technologist
        val wave = "👋🏽" // waving hand, medium skin tone
        val flag = "🇺🇸" // US
        val keycap = "#️⃣"
        assertEquals(
            listOf(
                emoji(family, "1f469_200d_1f4bb"), emoji(wave, "1f44b_1f3fd"),
                emoji(flag, "1f1fa_1f1f8"), emoji(keycap, "0023_20e3"),
            ),
            split(family + wave + flag + keycap),
        )
    }

    @Test
    fun `an emoji the pack lacks stays text for the system font`() {
        val unicorn = "🦄"
        assertEquals(listOf(EmojiRun.Text("a" + unicorn + "b")), split("a" + unicorn + "b"))
        assertTrue(EmojiSequences.isPlain(split(unicorn)))
    }

    @Test
    fun `an aliased flag draws with its target`() {
        val bouvet = "🇧🇻"
        assertEquals(listOf(emoji(bouvet, "1f1f3_1f1f4")), split(bouvet))
    }

    @Test
    fun `empty input is no runs`() {
        assertEquals(emptyList<EmojiRun>(), split(""))
    }
}
