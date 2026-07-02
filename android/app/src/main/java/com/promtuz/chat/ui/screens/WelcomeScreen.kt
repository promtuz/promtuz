package com.promtuz.chat.ui.screens

import android.app.Activity
import android.content.Intent
import android.widget.Toast
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.FocusDirection
import androidx.compose.ui.graphics.RectangleShape
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.R
import com.promtuz.chat.presentation.state.WelcomeField
import com.promtuz.chat.presentation.state.WelcomeStatus
import com.promtuz.chat.presentation.viewmodel.WelcomeVM
import com.promtuz.chat.ui.activities.App
import com.promtuz.chat.ui.components.OutlinedFormElements
import com.promtuz.chat.ui.constants.Buttonimations
import com.promtuz.chat.ui.constants.Tweens
import com.promtuz.chat.utils.extensions.then

@Composable
fun WelcomeScreen(
    welcomeViewModel: WelcomeVM
) {
    val colors = MaterialTheme.colorScheme
    val typography = MaterialTheme.typography
    val context = LocalContext.current

    val state by welcomeViewModel.uiState
    val isTryingToContinue by remember { derivedStateOf { state.status != WelcomeStatus.Normal } }
    val isNormal by remember { derivedStateOf { state.status == WelcomeStatus.Normal } }

    val focusManager = LocalFocusManager.current

    Box(
        Modifier
            .fillMaxSize()
            .background(colors.background)
            .verticalScroll(rememberScrollState())
            .pointerInput(Unit) {
                detectTapGestures { focusManager.clearFocus() }
            }) {

        Column(
            Modifier
                .fillMaxWidth()
                .padding(start = 24.dp, end = 24.dp, top = 120.dp, bottom = 0.dp)
                .align(Alignment.BottomCenter),
        ) {
            Text(
                stringResource(R.string.welcome_screen_title),
                fontWeight = FontWeight.Bold,
                style = typography.displayMedium,
                color = colors.onBackground
            )

            Spacer(Modifier.height(32.dp))

            Text(
                stringResource(R.string.welcome_screen_name_label),
                style = typography.bodyMedium,
                color = colors.onSurfaceVariant,
                modifier = Modifier.padding(bottom = 10.dp)
            )

            OutlinedFormElements.TextField(
                value = state.name,
                onValueChange = { welcomeViewModel.onChange(WelcomeField.Name, it) },
                placeholder = stringResource(R.string.welcome_screen_example_name),
                enabled = !isTryingToContinue,
                readOnly = isTryingToContinue,
                keyboardOptions = KeyboardOptions(
                    imeAction = ImeAction.Next
                ),
                keyboardActions = KeyboardActions(
                    onNext = { focusManager.moveFocus(FocusDirection.Next) }),
            )

            Spacer(Modifier.height(6.dp))

            Spacer(Modifier.height(6.dp))

            AnimatedContent(
                state.errorText,
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(4.dp),
                transitionSpec = {
                    (slideInVertically(
                        initialOffsetY = { fullHeight -> fullHeight },
                        animationSpec = Tweens.microInteraction()
                    ) + fadeIn(Tweens.microInteraction())) togetherWith (slideOutVertically(
                        targetOffsetY = { fullHeight -> fullHeight },
                        animationSpec = Tweens.microInteraction()
                    ) + fadeOut(Tweens.microInteraction()))
                }) { text ->

                Text(
                    text ?: "", fontSize = 14.sp, color = colors.error
                )

            }

            Button(
                onClick = {
                    isNormal.then {
                        welcomeViewModel.`continue` {
                            context.startActivity(Intent(context, App::class.java))
                            (context as? Activity)?.finish()
                        }
                    }
                },
                Modifier.fillMaxWidth(),
            ) {
                AnimatedContent(
                    state.status,
                    modifier = Modifier
                        .fillMaxWidth()
                        .clip(RectangleShape),
                    contentAlignment = Alignment.Center,
                    transitionSpec = { Buttonimations.labelSlide() }) { status ->
                    Text(
                        stringResource(status.text),
                        textAlign = TextAlign.Center,
                        fontWeight = FontWeight.W500,
                        fontSize = 16.sp,
                        modifier = Modifier.graphicsLayer { // Allow overflow
                            clip = false
                        })
                }
            }


            Spacer(Modifier.height(48.dp))

            Text(
                stringResource(R.string.welcome_screen_continue_existing_label),
                style = typography.bodyMedium,
                modifier = Modifier.padding(bottom = 4.dp),
                color = colors.onSurfaceVariant,
            )

            OutlinedButton(
                {
                    Toast.makeText(
                        context, "Importing is not supported yet.", Toast.LENGTH_SHORT
                    ).show()
                }, Modifier.fillMaxWidth()
            ) {
                Text(
                    stringResource(R.string.welcome_screen_continue_existing_button),
                    fontWeight = FontWeight.W500,
                    style = typography.labelLarge
                )
            }

            Box(
                Modifier
                    .fillMaxWidth()
                    .padding(top = 82.dp)
            ) {
                End2EndEncrypted(
                    Modifier
                        .align(Alignment.Center)
                        .padding(bottom = 42.dp)
                )
            }
        }
    }
}

@Composable
fun End2EndEncrypted(modifier: Modifier = Modifier) {
    val colors = MaterialTheme.colorScheme

    Row(
        modifier = modifier,
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(6.dp)
    ) {
        Icon(
            painter = painterResource(R.drawable.i_encrypted),
            "Encrypted",
            Modifier.size(16.dp),
            tint = colors.onSurface
        )

        Text(
            stringResource(R.string.e2ee),
            fontSize = 12.sp,
            fontWeight = FontWeight.W500,
            color = colors.onSurface
        )
    }
}