package com.promtuz.chat.ui.components

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import kotlin.random.Random

class AlbumLayoutTest {
    @Test
    fun everyCountAndShapeGetsOneCellPerPicture() {
        val rnd = Random(7)
        val shapes = listOf(0.5f, 0.6667f, 0.75f, 1f, 1.333f, 1.5f, 1.78f, 2.5f)
        repeat(400) {
            val count = rnd.nextInt(1, 16)
            val ratios = List(count) { shapes[rnd.nextInt(shapes.size)] }
            val cells = albumLayout(ratios)
            assertEquals("ratios $ratios", count, cells.size)
            cells.forEach { c ->
                assertTrue("cell $ratios", c.w > 0f && c.h > 0f && c.x >= -0.001f && c.y >= -0.001f)
                assertTrue("cell overflow $ratios", c.x + c.w <= 1.001f && c.y + c.h <= 4f)
            }
        }
    }
}
