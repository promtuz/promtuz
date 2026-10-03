package com.promtuz.chat.ui.animation

import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedContentScope
import androidx.compose.animation.AnimatedContentTransitionScope
import androidx.compose.animation.ContentTransform
import androidx.compose.animation.SizeTransform
import androidx.compose.animation.core.MutableTransitionState
import androidx.compose.animation.core.rememberTransition
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import com.promtuz.chat.ui.constants.Tweens

/**
 * Finishes each visible transition before advancing to the latest target; a mid-animation
 * change gets a [minDurationMillis] catch-up hop. Render the value passed to [content].
 */
@Composable
fun <T> QueuedAnimatedContent(
    targetState: T,
    modifier: Modifier = Modifier,
    durationMillis: Int = 300,
    minDurationMillis: Int = minOf(80, durationMillis),
    progression: ContentProgression<T>? = null,
    contentAlignment: Alignment = Alignment.CenterStart,
    label: String = "queued content",
    transitionSpec: AnimatedContentTransitionScope<T>.(durationMillis: Int) -> ContentTransform = {
        queuedVerticalTransition(it)
    },
    content: @Composable AnimatedContentScope.(T) -> Unit,
) {
    require(durationMillis > 0 && minDurationMillis in 1..durationMillis)
    val state = remember { MutableTransitionState(targetState) }
    val queue = remember { ContentQueue(targetState) }
    val requested by rememberUpdatedState(targetState)
    val baseDuration by rememberUpdatedState(durationMillis)
    val minimumDuration by rememberUpdatedState(minDurationMillis)
    val currentProgression by rememberUpdatedState(progression)
    var hopDuration by remember { mutableIntStateOf(durationMillis) }
    val transition = rememberTransition(state, label)

    // Read only target changes and the idle boundary, so animation frames don't drive queue work.
    LaunchedEffect(state) {
        // Include the settled value: with animations disabled, Compose can pass
        // through busy -> idle before the collector observes the busy snapshot.
        snapshotFlow { Triple(requested, state.isIdle, state.currentState) }.collect { (value, idle, _) ->
            queue.offer(value)
            if (idle) {
                queue.complete()
                queue.next(baseDuration, minimumDuration, currentProgression)?.let { hop ->
                    hopDuration = hop.durationMillis
                    state.targetState = hop.target
                }
            }
        }
    }

    transition.AnimatedContent(
        modifier = modifier,
        contentAlignment = contentAlignment,
        transitionSpec = { transitionSpec(hopDuration) },
        content = content,
    )
}

/** Shared entry, exit and size timing; no delayed fade that exposes a blank label. */
fun <T> AnimatedContentTransitionScope<T>.queuedVerticalTransition(durationMillis: Int, clip: Boolean = true): ContentTransform =
    ((slideInVertically(Tweens.microInteraction(durationMillis)) { it } + fadeIn(Tweens.microInteraction(durationMillis))) togetherWith
        (slideOutVertically(Tweens.microInteraction(durationMillis)) { -it } + fadeOut(Tweens.microInteraction(durationMillis))))
        .using(SizeTransform(clip = clip, sizeAnimationSpec = { _, _ -> Tweens.microInteraction(durationMillis) }))
