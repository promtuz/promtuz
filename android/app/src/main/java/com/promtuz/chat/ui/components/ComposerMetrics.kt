package com.promtuz.chat.ui.components

import androidx.compose.animation.core.Animatable
import androidx.compose.runtime.Composable
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue

/**
 * The composer's live footprint, published for the chat stage so the bar's height
 * and the stage's bottom anchor resolve from the same numbers.
 *
 * [progress] is the reveal's own animation rather than a measurement of it: the
 * stage reads the same object the bar animates, so the two are locked by
 * construction instead of by a measure round-trip. The measured parts move only
 * on discrete events — a wrapped input line, the keyboard, the attach panel.
 */
@Stable
class ComposerMetrics {
    /** The glass bar with the action block closed, outer margins included. */
    var composerPx by mutableIntStateOf(0)
        internal set

    /** What [AttachPanel] holds below the bar: keyboard, panel, or nav bar. */
    var regionPx by mutableIntStateOf(0)
        internal set

    /** The action block at full height, measured whether or not it's open. */
    var actionH by mutableIntStateOf(0)
        internal set

    /** The media strip at full height, measured whether or not it's open. */
    var stripH by mutableIntStateOf(0)
        internal set

    /** Action reveal progress, 0 closed → 1 open. */
    val progress = Animatable(0f)

    /** Media strip reveal progress, on the same clock. */
    val stripProgress = Animatable(0f)

    val actionPx: Float get() = actionH * progress.value
    val stripPx: Float get() = stripH * stripProgress.value

    /**
     * The stage's push share: growth the messages ride up with, rather than
     * merely being covered by. Both of these are the composer expanding because
     * the user staged something for *this* message, so they displace at any
     * scroll position — unlike the keyboard, which only covers.
     */
    val pushPx: Float get() = actionPx + stripPx

    /** Total bottom inset the stage reserves under the messages. */
    val bottomPx: Float get() = composerPx + regionPx + pushPx
}

@Composable
fun rememberComposerMetrics(): ComposerMetrics = remember { ComposerMetrics() }
