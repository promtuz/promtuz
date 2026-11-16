package com.promtuz.chat.ui.components

import androidx.compose.foundation.background
import android.view.ViewGroup
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.EaseOutQuint
import androidx.compose.animation.core.tween
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.window.DialogProperties
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.theme.Radii
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.unit.LayoutDirection
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogWindowProvider
import androidx.compose.ui.semantics.paneTitle
import androidx.compose.ui.semantics.semantics
import com.promtuz.chat.R
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/** Fades only: a window scale would drag the blurred backdrop along with it. Scale runs in Compose instead. */
@Composable
internal fun ModalWindowMotion() {
    val view = LocalView.current
    SideEffect {
        (view.parent as? DialogWindowProvider)?.window?.setWindowAnimations(R.style.AppModalAnimation)
    }
}

/**
 * A full-screen window with its own scrim, so the frosted panel can blur the app behind it and the
 * scrim tap and system bars behave the same on every modal.
 */
@Composable
internal fun AppModal(
    onDismissRequest: () -> Unit,
    maxWidth: Dp = 400.dp,
    content: @Composable () -> Unit,
) {
    Dialog(onDismissRequest, DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false)) {
        val view = LocalView.current
        val dark = Promtuz.colors.isDark
        SideEffect {
            (view.parent as? DialogWindowProvider)?.window?.apply {
                setWindowAnimations(R.style.AppModalAnimation)
                setDimAmount(0f)
                setLayout(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT)
                drawEdgeToEdge(view, dark)
            }
        }
        val scale = remember { Animatable(0.94f) }
        LaunchedEffect(Unit) { scale.animateTo(1f, tween(220, easing = EaseOutQuint)) }
        val dismiss by rememberUpdatedState(onDismissRequest)
        Box(
            Modifier.fillMaxSize()
                .background(ModalScrim)
                .pointerInput(Unit) { detectTapGestures { dismiss() } }
                .systemBarsPadding().imePadding().padding(24.dp),
            contentAlignment = Alignment.Center,
        ) {
            Box(
                Modifier.widthIn(max = maxWidth)
                    .graphicsLayer { scaleX = scale.value; scaleY = scale.value }
                    .pointerInput(Unit) { detectTapGestures { } }
                    .semantics { paneTitle = "Dialog" },
                propagateMinConstraints = true,
            ) {
                CompositionLocalProvider(LocalOnGlass provides true, LocalContentColor provides Promtuz.colors.text) { content() }
            }
        }
    }
}

val ModalScrim = Color.Black.copy(alpha = 0.2f)

/** A window that paints nothing behind the system bars, with icons that suit the app theme rather than the system's. */
@Suppress("DEPRECATION")
fun android.view.Window.drawEdgeToEdge(view: android.view.View, dark: Boolean) {
    addFlags(android.view.WindowManager.LayoutParams.FLAG_DRAWS_SYSTEM_BAR_BACKGROUNDS)
    clearFlags(android.view.WindowManager.LayoutParams.FLAG_TRANSLUCENT_STATUS or android.view.WindowManager.LayoutParams.FLAG_TRANSLUCENT_NAVIGATION)
    statusBarColor = android.graphics.Color.TRANSPARENT
    navigationBarColor = android.graphics.Color.TRANSPARENT
    if (android.os.Build.VERSION.SDK_INT >= 29) isNavigationBarContrastEnforced = false
    androidx.core.view.WindowCompat.setDecorFitsSystemWindows(this, false)
    androidx.core.view.WindowCompat.getInsetsController(this, view).apply {
        isAppearanceLightStatusBars = !dark
        isAppearanceLightNavigationBars = !dark
    }
}
val ModalShape = RoundedCornerShape(Radii.modal)

/** Material button ordering/wrapping; only the footer's outer padding is reduced. */
@Composable
internal fun ModalActions(confirmButton: @Composable () -> Unit, dismissButton: (@Composable () -> Unit)?) {
    val direction = LocalLayoutDirection.current
    // The buttons sit about 8dp in from the dialog's edge (the footer's padding plus their touch margin).
    Box(Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 4.dp), contentAlignment = Alignment.CenterEnd) {
        CompositionLocalProvider(LocalTextButtonShape provides RoundedCornerShape(concentric(Radii.modal, 8.dp))) {
        CompositionLocalProvider(LocalLayoutDirection provides
            if (direction == LayoutDirection.Ltr) LayoutDirection.Rtl else LayoutDirection.Ltr) {
            FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalArrangement = Arrangement.spacedBy(0.dp)) {
                CompositionLocalProvider(LocalLayoutDirection provides direction) {
                    confirmButton()
                    dismissButton?.invoke()
                }
            }
        }
        }
    }
}

@Composable
fun AppAlertDialog(
    onDismissRequest: () -> Unit,
    confirmButton: @Composable () -> Unit,
    modifier: Modifier = Modifier,
    dismissButton: (@Composable () -> Unit)? = null,
    icon: (@Composable () -> Unit)? = null,
    title: (@Composable () -> Unit)? = null,
    text: (@Composable () -> Unit)? = null,
) {
    AppModal(onDismissRequest) {
        Column(modifier.floatingSurface(ModalShape)) {
            Column(Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState())
                .padding(start = 24.dp, end = 24.dp, top = 24.dp)) {
                icon?.let {
                    CompositionLocalProvider(LocalContentColor provides Promtuz.colors.accent) {
                        Box(Modifier.align(Alignment.CenterHorizontally).padding(bottom = 12.dp)) { it() }
                    }
                }
                title?.let {
                    CompositionLocalProvider(LocalContentColor provides Promtuz.colors.text) {
                        ProvideTextStyle(MaterialTheme.typography.titleLargeEmphasized.copy(fontWeight = FontWeight.Medium)) {
                            Box(Modifier.align(if (icon == null) Alignment.Start else Alignment.CenterHorizontally)
                                .padding(bottom = 12.dp)) { it() }
                        }
                    }
                }
                text?.let {
                    CompositionLocalProvider(LocalContentColor provides Promtuz.colors.text) {
                        ProvideTextStyle(MaterialTheme.typography.bodyMediumEmphasized.copy(lineHeight = 18.5.sp)) { it() }
                    }
                }
            }
            ModalActions(confirmButton, dismissButton)
        }
    }
}
