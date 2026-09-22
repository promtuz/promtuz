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
 * underscores. Anything the pack
 * lacks stays text and falls back to the system font, so a newer emoji than the
 * pack degrades to what the device draws today rather than to a box.
 */
object EmojiSequences {
    private const val VS15 = 0xFE0E
    private const val VS16 = 0xFE0F
    private const val ZWJ = 0x200D
    private const val KEYCAP = 0x20E3

    /**
     * Basic-plane characters that are emoji even without a variation selector
     * (Unicode `Emoji_Presentation=Yes` below U+1F000). Everything else on the
     * basic plane is text unless the cluster carries U+FE0F.
     */
    private val bmpEmojiPresentation: Set<Int> = buildSet {
        addAll(0x231A..0x231B); addAll(0x23E9..0x23EC); add(0x23F0); add(0x23F3)
        addAll(0x25FD..0x25FE); addAll(0x2614..0x2615); addAll(0x2648..0x2653)
        add(0x267F); add(0x2693); add(0x26A1); addAll(0x26AA..0x26AB); addAll(0x26BD..0x26BE)
        addAll(0x26C4..0x26C5); add(0x26CE); add(0x26D4); add(0x26EA); addAll(0x26F2..0x26F3)
        add(0x26F5); add(0x26FA); add(0x26FD); add(0x2705); addAll(0x270A..0x270B)
        add(0x2728); add(0x274C); add(0x274E); addAll(0x2753..0x2755); add(0x2757)
        addAll(0x2795..0x2797); add(0x27B0); add(0x27BF); addAll(0x2B1B..0x2B1C)
        add(0x2B50); add(0x2B55)
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

    /**
     * Whether a cluster is an emoji rather than a symbol that merely has a
     * picture in the pack. Supplementary-plane pictographs, anything joined or
     * keycapped or carrying U+FE0F, and the basic-plane set above count; a bare
     * `©`, `™` or `↔` does not.
     */
    fun looksLikeEmoji(cluster: String): Boolean {
        var i = 0
        while (i < cluster.length) {
            val cp = cluster.codePointAt(i)
            i += Character.charCount(cp)
            when {
                cp == VS16 || cp == ZWJ || cp == KEYCAP -> return true
                cp in 0x1F000..0x1FAFF -> return true
                cp in bmpEmojiPresentation -> return true
            }
        }
        return false
    }

    private fun resolve(key: String, pack: Set<String>, aliases: Map<String, String>): String? {
        if (key in pack) return key
        val target = aliases[key] ?: return null
        return target.takeIf { it in pack }
    }
}
