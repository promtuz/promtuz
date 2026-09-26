package com.promtuz.chat.ui.text

/** A run of text: plain, or one emoji cluster with the pack key that draws it. */
sealed interface EmojiRun {
    data class Text(val text: String) : EmojiRun
    data class Emoji(val cluster: String, val key: String) : EmojiRun
}

/**
 * Splits text into plain runs and the emoji clusters a bundled pack can draw.
 *
 * Sequences are scanned by the emoji grammar itself (a ZWJ family, a
 * skin-toned hand, a keycap or a flag pair is one unit), then judged: a bare
 * `©` is text, `©️` is not. A matched sequence is named by the pack's key
 * scheme: codepoints in four-digit hex joined by underscores, variation
 * selectors dropped, which is what build-emoji-pack.py writes.
 * Anything the pack lacks stays text and falls back to the system font, so a newer emoji than the
 * pack degrades to what the device draws today rather than to a box.
 */
object EmojiSequences {
    private const val VS15 = 0xFE0E
    private const val VS16 = 0xFE0F
    private const val ZWJ = 0x200D
    private const val KEYCAP = 0x20E3

    /**
     * Unicode 17.0 Emoji_Presentation=Yes, including supplementary-plane defaults.
     * Source: https://www.unicode.org/Public/17.0.0/ucd/emoji/emoji-data.txt
     * © 2025 Unicode, Inc. License: tools/licenses/notices/unicode.txt.
     * Keep this pinned so classification does not depend on the device's Unicode version.
     */
    private val emojiPresentation: Set<Int> = buildSet {
        addAll(0x231A..0x231B); addAll(0x23E9..0x23EC); add(0x23F0)
        add(0x23F3); addAll(0x25FD..0x25FE); addAll(0x2614..0x2615)
        addAll(0x2648..0x2653); add(0x267F); add(0x2693)
        add(0x26A1); addAll(0x26AA..0x26AB); addAll(0x26BD..0x26BE)
        addAll(0x26C4..0x26C5); add(0x26CE); add(0x26D4)
        add(0x26EA); addAll(0x26F2..0x26F3); add(0x26F5)
        add(0x26FA); add(0x26FD); add(0x2705)
        addAll(0x270A..0x270B); add(0x2728); add(0x274C)
        add(0x274E); addAll(0x2753..0x2755); add(0x2757)
        addAll(0x2795..0x2797); add(0x27B0); add(0x27BF)
        addAll(0x2B1B..0x2B1C); add(0x2B50); add(0x2B55)
        add(0x1F004); add(0x1F0CF); add(0x1F18E)
        addAll(0x1F191..0x1F19A); addAll(0x1F1E6..0x1F1FF); add(0x1F201)
        add(0x1F21A); add(0x1F22F); addAll(0x1F232..0x1F236)
        addAll(0x1F238..0x1F23A); addAll(0x1F250..0x1F251); addAll(0x1F300..0x1F320)
        addAll(0x1F32D..0x1F335); addAll(0x1F337..0x1F37C); addAll(0x1F37E..0x1F393)
        addAll(0x1F3A0..0x1F3CA); addAll(0x1F3CF..0x1F3D3); addAll(0x1F3E0..0x1F3F0)
        add(0x1F3F4); addAll(0x1F3F8..0x1F43E); add(0x1F440)
        addAll(0x1F442..0x1F4FC); addAll(0x1F4FF..0x1F53D); addAll(0x1F54B..0x1F54E)
        addAll(0x1F550..0x1F567); add(0x1F57A); addAll(0x1F595..0x1F596)
        add(0x1F5A4); addAll(0x1F5FB..0x1F64F); addAll(0x1F680..0x1F6C5)
        add(0x1F6CC); addAll(0x1F6D0..0x1F6D2); addAll(0x1F6D5..0x1F6D8)
        addAll(0x1F6DC..0x1F6DF); addAll(0x1F6EB..0x1F6EC); addAll(0x1F6F4..0x1F6FC)
        addAll(0x1F7E0..0x1F7EB); add(0x1F7F0); addAll(0x1F90C..0x1F93A)
        addAll(0x1F93C..0x1F945); addAll(0x1F947..0x1F9FF); addAll(0x1FA70..0x1FA7C)
        addAll(0x1FA80..0x1FA8A); addAll(0x1FA8E..0x1FAC6); add(0x1FAC8)
        addAll(0x1FACD..0x1FADC); addAll(0x1FADF..0x1FAEA); addAll(0x1FAEF..0x1FAF8)
    }

    /** Split [text] against a pack of asset keys, plus any `from>to` aliases the index carries. */
    fun split(
        text: String, pack: Set<String>, aliases: Map<String, String> = emptyMap(),
    ): List<EmojiRun> {
        if (text.isEmpty()) return emptyList()
        val runs = ArrayList<EmojiRun>()
        val plain = StringBuilder()
        var i = 0
        while (i < text.length) {
            val end = clusterEnd(text, i)
            val cluster = text.substring(i, end)
            val key = if (looksLikeEmoji(cluster)) resolve(keyOf(cluster), pack, aliases) else null
            if (key == null) {
                plain.append(cluster)
            } else {
                if (plain.isNotEmpty()) {
                    runs += EmojiRun.Text(plain.toString())
                    plain.setLength(0)
                }
                runs += EmojiRun.Emoji(cluster, key)
            }
            i = end
        }
        if (plain.isNotEmpty()) runs += EmojiRun.Text(plain.toString())
        return runs
    }

    /**
     * Where the emoji sequence starting at [start] ends, per the emoji grammar
     * rather than the platform's grapheme segmenter: a base, then any run of
     * variation selectors, skin tones, a keycap or tag characters, then as many
     * `ZWJ + element` links as follow; two regional indicators pair into a flag.
     * Text that is not emoji yields one codepoint at a time, which is all the
     * caller needs from it. Scanning it ourselves keeps the JVM tests and the
     * device (whose segmenters disagree on the edges) on one behaviour.
     */
    private fun clusterEnd(text: String, start: Int): Int {
        var i = start
        val base = text.codePointAt(i)
        i += Character.charCount(base)
        if (isRegionalIndicator(base) && i < text.length && isRegionalIndicator(text.codePointAt(i))) {
            return i + Character.charCount(text.codePointAt(i))
        }
        i = absorbModifiers(text, i)
        while (i < text.length && text.codePointAt(i) == ZWJ) {
            val after = i + 1
            if (after >= text.length) break
            val next = text.codePointAt(after)
            i = absorbModifiers(text, after + Character.charCount(next))
        }
        return i
    }

    private fun absorbModifiers(text: String, from: Int): Int {
        var i = from
        while (i < text.length) {
            val cp = text.codePointAt(i)
            val modifier = cp == VS15 || cp == VS16 || cp == KEYCAP ||
                cp in 0x1F3FB..0x1F3FF || cp in 0xE0020..0xE007F
            if (!modifier) break
            i += Character.charCount(cp)
        }
        return i
    }

    private fun isRegionalIndicator(cp: Int) = cp in 0x1F1E6..0x1F1FF

    /** True when [runs] holds nothing the pack draws, so the caller can skip the inline machinery. */
    fun isPlain(runs: List<EmojiRun>): Boolean = runs.none { it is EmojiRun.Emoji }

    /** The pack key for a cluster: hex codepoints padded to four digits, variation selectors dropped, `_`-joined. */
    fun keyOf(cluster: String): String {
        val sb = StringBuilder()
        var i = 0
        while (i < cluster.length) {
            val cp = cluster.codePointAt(i)
            i += Character.charCount(cp)
            if (cp == VS15 || cp == VS16) continue
            if (sb.isNotEmpty()) sb.append('_')
            sb.append(Integer.toHexString(cp).padStart(4, '0'))
        }
        return sb.toString()
    }

    /** Explicit text presentation wins; otherwise use emoji sequences or Unicode defaults. */
    fun looksLikeEmoji(cluster: String): Boolean {
        var emoji = false
        var i = 0
        while (i < cluster.length) {
            val cp = cluster.codePointAt(i)
            i += Character.charCount(cp)
            // Inspect the entire cluster before accepting it: a selector follows its base,
            // and a joined sequence can contain a text selector in a later element.
            if (cp == VS15) return false
            if (cp == VS16 || cp == ZWJ || cp == KEYCAP || cp in emojiPresentation) emoji = true
        }
        return emoji
    }

    private fun resolve(key: String, pack: Set<String>, aliases: Map<String, String>): String? {
        if (key in pack) return key
        val target = aliases[key] ?: return null
        return target.takeIf { it in pack }
    }
}
