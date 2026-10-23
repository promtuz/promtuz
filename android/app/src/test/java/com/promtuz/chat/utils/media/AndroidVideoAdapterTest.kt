package com.promtuz.chat.utils.media

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class AndroidVideoAdapterTest {
    @Test fun variableFrameRateTimelineUsesPresentationTimestamps() {
        val source = buildList {
            repeat(10) { add(it * 100_000L) }
            repeat(30) { add(1_000_000L + it * 33_333L) }
        }
        val rendered = mutableListOf<Long>()
        source.forEach { presentationTimeUs ->
            if (shouldRenderVideoFrame(
                    presentationTimeUs,
                    firstPresentationTimeUs = 0,
                    lastRenderedUs = rendered.lastOrNull() ?: Long.MIN_VALUE,
                    minimumFrameDurationUs = 1_000_000L / 15,
                )
            ) rendered += presentationTimeUs
        }

        assertEquals((0..9).map { it * 100_000L }, rendered.take(10))
        assertEquals(15, rendered.drop(10).size)
        assertTrue(rendered.zipWithNext().all { (before, after) -> after > before })
    }

    @Test fun nearSixtyFpsDoesNotDropToTwentyFps() {
        var lastRenderedUs = Long.MIN_VALUE
        val rendered = (0 until 120).count { index ->
            val timestampUs = index * 16_633L
            shouldRenderVideoFrame(timestampUs, 0, lastRenderedUs, 1_000_000L / 30).also {
                if (it) lastRenderedUs = timestampUs
            }
        }
        assertEquals(60, rendered)
    }
}
