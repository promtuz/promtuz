package com.promtuz.chat.ui.constants

import androidx.compose.animation.ContentTransform
import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.animation.core.EaseInOutCirc
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.TweenSpec
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith

object Tweens {
    fun <T> microInteraction(dur: Int = 150): TweenSpec<T> {
        return tween(dur, easing = EaseInOutCirc)
    }
}

object Buttonimations {
    fun labelSlide(): ContentTransform {
        return (slideInVertically(
            initialOffsetY = { fullHeight -> fullHeight }, animationSpec = Tweens.microInteraction()
        ) + fadeIn(Tweens.microInteraction())) togetherWith (slideOutVertically(
            targetOffsetY = { fullHeight -> -fullHeight }, animationSpec = Tweens.microInteraction()
        ) + fadeOut(Tweens.microInteraction()))
    }
}

object Naviganimation {
    private const val DUR = 300
    private val ease = CubicBezierEasing(0.2f, 0.8f, 0.2f, 1f)

    // Forward: new screen slides in full-width from the right, over the old; old parallax-drifts left.
    fun transitionSpec() = ContentTransform(
        targetContentEnter = slideInHorizontally(tween(DUR, easing = ease)) { it },
        initialContentExit = slideOutHorizontally(tween(DUR, easing = ease)) { -it / 4 },
        targetContentZIndex = 1f,
    )

    // Back: current slides off to the right, revealing the previous (parallax in from the left).
    fun popTransitionSpec() = ContentTransform(
        targetContentEnter = slideInHorizontally(tween(DUR, easing = ease)) { -it / 4 },
        initialContentExit = slideOutHorizontally(tween(DUR, easing = ease)) { it },
        targetContentZIndex = 0f,
    )

    // Interactive swipe-back: same arrangement as pop, but LINEAR so nav3's seek tracks the
    // finger 1:1 (an eased curve would run the screen ahead of the finger during the drag).
    fun predictivePopSpec() = ContentTransform(
        targetContentEnter = slideInHorizontally(tween(DUR, easing = LinearEasing)) { -it / 4 },
        initialContentExit = slideOutHorizontally(tween(DUR, easing = LinearEasing)) { it },
        targetContentZIndex = 0f,
    )
}
