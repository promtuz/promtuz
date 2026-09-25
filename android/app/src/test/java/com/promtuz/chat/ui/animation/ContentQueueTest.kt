package com.promtuz.chat.ui.animation

import org.junit.Assert.*
import org.junit.Test

class ContentQueueTest {
    @Test fun fastConnectionFinishesActiveHopThenCatchesUpToLatest() {
        val queue = ContentQueue("Home")
        queue.offer("Connecting")
        assertEquals(ContentQueue.Hop("Connecting", 300), queue.next(300, 80, null))
        // Requests can arrive before Compose has even started drawing the hop.
        queue.offer("Handshaking")
        queue.offer("Connected")
        assertNull(queue.next(300, 80, null))
        queue.complete()
        assertEquals(ContentQueue.Hop("Connected", 80), queue.next(300, 80, null))
        queue.complete()
        assertNull(queue.next(300, 80, null))
        queue.offer("Home")
        assertEquals(ContentQueue.Hop("Home", 300), queue.next(300, 80, null))
    }

    @Test fun returningToActiveTargetDiscardsStalePendingValue() {
        val queue = ContentQueue("A")
        queue.offer("B")
        queue.next(300, 80, null)
        queue.offer("C")
        queue.offer("B")
        queue.complete()
        assertNull(queue.next(300, 80, null))
        queue.offer("D")
        assertEquals(300, queue.next(300, 80, null)!!.durationMillis)
    }

    @Test fun nullableLabelCanBeQueuedAndReverseAfterCompletion() {
        val queue = ContentQueue<String?>(null)
        queue.offer("Connecting")
        queue.next(300, 80, null)
        queue.offer(null)
        assertNull(queue.next(300, 80, null))
        queue.complete()
        val hop = queue.next(300, 80, null)
        assertNotNull(hop)
        assertNull(hop!!.target)
        queue.complete()
        assertNull(queue.next(300, 80, null))
    }

    @Test fun largeCounterGapShowsEveryValueFasterThenSettles() {
        val progression = ContentProgression.Integers
        val near = ContentQueue(1).apply { offer(3) }.next(300, 80, progression)!!
        val queue = ContentQueue(1).apply { offer(10) }
        val hops = mutableListOf<ContentQueue.Hop<Int>>()
        while (true) {
            val hop = queue.next(300, 80, progression) ?: break
            hops += hop
            queue.complete()
        }
        assertEquals((2..10).toList(), hops.map { it.target })
        assertTrue(hops.first().durationMillis < near.durationMillis)
        assertEquals(80, hops.first().durationMillis)
        assertEquals(300, hops.last().durationMillis)
        assertTrue(hops.zipWithNext().all { (a, b) -> a.durationMillis <= b.durationMillis })
    }

    @Test fun extendingAndReversingCounterNeverRetargetsActiveHop() {
        val queue = ContentQueue(1)
        val progression = ContentProgression.Integers
        queue.offer(2)
        val first = queue.next(300, 80, progression)!!
        queue.offer(10)
        assertNull(queue.next(300, 80, progression))
        assertEquals(ContentQueue.Hop(2, 300), first)
        queue.complete()
        assertEquals(ContentQueue.Hop(3, 80), queue.next(300, 80, progression))
        queue.offer(0)
        assertNull(queue.next(300, 80, progression))
        queue.complete()
        assertEquals(ContentQueue.Hop(2, 100), queue.next(300, 80, progression))
        queue.complete()
        assertEquals(ContentQueue.Hop(1, 150), queue.next(300, 80, progression))
        queue.complete()
        assertEquals(ContentQueue.Hop(0, 300), queue.next(300, 80, progression))
    }

    @Test fun counterDistanceDoesNotOverflowOrAllocateTheBacklog() {
        val progression = ContentProgression.Integers
        assertEquals(4_294_967_295L, progression.remainingSteps(Int.MIN_VALUE, Int.MAX_VALUE))
        val queue = ContentQueue(Int.MAX_VALUE).apply { offer(Int.MIN_VALUE) }
        assertEquals(ContentQueue.Hop(Int.MAX_VALUE - 1, 80), queue.next(300, 80, progression))
    }
}
