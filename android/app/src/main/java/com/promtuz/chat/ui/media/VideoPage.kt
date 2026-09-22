package com.promtuz.chat.ui.media

import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.media3.common.MediaItem as ExoMediaItem
import androidx.media3.common.Player
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.ui.compose.PlayerSurface
import androidx.media3.ui.compose.SURFACE_TYPE_TEXTURE_VIEW
import com.promtuz.chat.R
import com.promtuz.chat.ui.components.LottieFrame
import com.promtuz.chat.ui.components.LottieLoop
import com.promtuz.chat.ui.components.rememberLottie
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.ui.res.painterResource
import kotlinx.coroutines.delay
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.StrokeCap

/** One page's player and what the controls need to know about it. */
class VideoPlayerState(val player: ExoPlayer, val path: String) {
    /** Set while another host is taking this player over, so the old host does not release it. */
    var handedOver = false
    var playing by mutableStateOf(false)
    var rendered by mutableStateOf(false)
    var ended by mutableStateOf(false)
    var buffering by mutableStateOf(false)
    var position by mutableLongStateOf(0L)
    var duration by mutableLongStateOf(0L)

    fun toggle() = when {
        ended -> { player.seekTo(0); player.play() }
        playing -> player.pause()
        else -> player.play()
    }
}

/** Plays while [active], pauses otherwise, and is released with the page. */
@Composable
fun rememberVideoPlayer(path: String, active: Boolean, adopt: VideoPlayerState? = null): VideoPlayerState {
    val context = LocalContext.current
    val state = remember(path) {
        // The surface moves with the player; the poster covers it until the first frame lands here.
        adopt?.takeIf { it.path == path }?.also { it.handedOver = false; it.rendered = false } ?: VideoPlayerState(
            ExoPlayer.Builder(context).build().apply {
                setMediaItem(ExoMediaItem.fromUri("file://$path"))
                repeatMode = Player.REPEAT_MODE_OFF
                prepare()
            },
            path,
        )
    }
    DisposableEffect(state) {
        val player = state.player
        val listener = object : Player.Listener {
            override fun onIsPlayingChanged(isPlaying: Boolean) { state.playing = isPlaying }
            override fun onRenderedFirstFrame() { state.rendered = true }
            override fun onPlaybackStateChanged(playbackState: Int) {
                if (playbackState == Player.STATE_READY) state.duration = player.duration.coerceAtLeast(0L)
                state.ended = playbackState == Player.STATE_ENDED
                state.buffering = playbackState == Player.STATE_BUFFERING
            }
        }
        player.addListener(listener)
        onDispose { player.removeListener(listener); if (!state.handedOver) player.release() }
    }
    LaunchedEffect(active) { if (active) state.player.play() else state.player.pause() }
    LaunchedEffect(state.playing) {
        while (state.playing) {
            state.position = state.player.currentPosition
            delay(200)
        }
        state.position = state.player.currentPosition
    }
    return state
}

/** The picture area: the poster until the first frame lands, so the flight in and the page match. */
@Composable
fun VideoSurface(state: VideoPlayerState, item: MediaItem, modifier: Modifier = Modifier) {
    // The first-frame callback lands a beat before the texture shows it, so the poster stays
    // a moment longer than the flag alone would keep it.
    var cover by remember(state) { mutableStateOf(true) }
    LaunchedEffect(state, state.rendered) {
        if (state.rendered) { delay(150); cover = false } else cover = true
    }
    Box(modifier) {
        PlayerSurface(state.player, Modifier.fillMaxSize(), surfaceType = SURFACE_TYPE_TEXTURE_VIEW)
        if (cover) item.thumb?.let {
            Image(it, null, Modifier.fillMaxSize(), contentScale = ContentScale.Fit)
        }
    }
}

/** Centre play button and the bottom scrubber, drawn over the whole page so zoom leaves them alone. */
@Composable
fun VideoControls(state: VideoPlayerState, chrome: Boolean, bottomInset: androidx.compose.ui.unit.Dp, modifier: Modifier = Modifier) {
    var scrub by remember { mutableFloatStateOf(-1f) }
    Box(modifier.fillMaxSize()) {
        // Hero: the play/pause morph parks on frame 0 (play) or 18 (pause) and reverses along
        // the same frames, so a quick double tap never restarts the motion.
        val heroFrame by animateFloatAsState(if (state.playing) 18f else 0f, tween(300, easing = LinearEasing), label = "hero")
        val hero = rememberLottie(R.raw.media_play_pause)
        Box(
            Modifier
                .align(Alignment.Center)
                .graphicsLayer { alpha = if (chrome || !state.playing) 1f else 0f }
                .size(72.dp)
                .pointerInput(state) { detectTapGestures { state.toggle() } },
            contentAlignment = Alignment.Center,
        ) {
            when {
                state.buffering && !state.ended -> LottieLoop(R.raw.media_buffering, Modifier.size(72.dp))
                state.ended -> Image(painterResource(R.drawable.ic_media_replay), "Replay", Modifier.size(72.dp))
                else -> LottieFrame(hero, { heroFrame }, Modifier.size(72.dp))
            }
        }

        if (chrome && state.duration > 0) Column(
            Modifier
                .align(Alignment.BottomCenter)
                .fillMaxWidth()
                .padding(horizontal = 16.dp)
                .padding(bottom = bottomInset),
        ) {
            val duration = state.duration
            val shown = if (scrub >= 0f) scrub else (state.position.toFloat() / duration).coerceIn(0f, 1f)
            Row(Modifier.fillMaxWidth().padding(bottom = 2.dp)) {
                Text(clock((shown * duration).toLong()), style = MaterialTheme.typography.labelSmall, color = Color.White)
                Box(Modifier.weight(1f))
                Text(clock(duration), style = MaterialTheme.typography.labelSmall, color = Color.White.copy(alpha = 0.7f))
            }
            Scrubber(
                fraction = shown,
                scrubbing = scrub >= 0f,
                onScrub = { scrub = it },
                onRelease = {
                    val at = (scrub * duration).toLong()
                    state.player.seekTo(at)
                    state.position = at
                    scrub = -1f
                },
            )
        }
    }
}

/** A hairline track with a small knob: the played part is white, the rest a faint white. */
@Composable
private fun Scrubber(fraction: Float, scrubbing: Boolean, onScrub: (Float) -> Unit, onRelease: () -> Unit) {
    val knob by animateFloatAsState(if (scrubbing) 1.5f else 1f, tween(120), label = "knob")
    Canvas(
        Modifier
            .fillMaxWidth()
            .height(24.dp)
            .pointerInput(Unit) {
                awaitEachGesture {
                    val down = awaitFirstDown()
                    down.consume()
                    onScrub((down.position.x / size.width).coerceIn(0f, 1f))
                    while (true) {
                        val ch = awaitPointerEvent().changes.first()
                        ch.consume()
                        if (!ch.pressed) break
                        onScrub((ch.position.x / size.width).coerceIn(0f, 1f))
                    }
                    onRelease()
                }
            },
    ) {
        val y = size.height / 2f
        val x = size.width * fraction
        val track = 2.dp.toPx()
        drawLine(Color.White.copy(alpha = 0.3f), Offset(0f, y), Offset(size.width, y), track, StrokeCap.Round)
        drawLine(Color.White, Offset(0f, y), Offset(x, y), track, StrokeCap.Round)
        drawCircle(Color.White, 5.dp.toPx() * knob, Offset(x, y))
    }
}

fun clock(ms: Long): String {
    val s = ms / 1000
    val h = s / 3600
    return if (h > 0) "%d:%02d:%02d".format(h, s / 60 % 60, s % 60) else "%d:%02d".format(s / 60, s % 60)
}
