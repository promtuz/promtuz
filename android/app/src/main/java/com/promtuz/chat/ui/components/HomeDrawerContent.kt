package com.promtuz.chat.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.*
import androidx.compose.ui.draw.*
import androidx.compose.ui.res.*
import androidx.compose.ui.unit.*
import androidx.navigation3.runtime.NavKey
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.text.avgSizeInStyle
import com.promtuz.chat.ui.util.groupedRoundShape
import kotlinx.coroutines.launch

private data class DrawerButton(val label: String, val icon: Int, val onClick: (() -> Unit)? = null)

@Composable
fun HomeDrawerContent(
    viewModel: AppVM, drawerState: DrawerState
) {
    val scope = rememberCoroutineScope()
    val open = remember {
        { route: NavKey ->
            scope.launch { drawerState.close() }
            viewModel.navigator.push(route)
        }
    }


    BoxWithConstraints {
        val maxWidth = maxWidth * 0.8f

        val drawerButtonGroups: List<List<DrawerButton>> = remember {
            listOf(
                listOf(
                    DrawerButton("My Profile", R.drawable.i_profile)
                ), listOf(
                    DrawerButton("Saved Users", R.drawable.i_contacts) { open(Routes.SavedUsers) },
                    DrawerButton("Blocked Users", R.drawable.i_user_blocked),
                ), listOf(
                    DrawerButton("Settings", R.drawable.oi_settings) { open(Routes.Settings) },
                    DrawerButton("About", R.drawable.oi_info) { open(Routes.About) },
                )
            )
        }

        ModalDrawerSheet(
            modifier = Modifier
                .widthIn(min = 200.dp, max = maxWidth)
                .fillMaxWidth()
        ) {
            LazyColumn(
                Modifier
                    .fillMaxWidth()
                    .padding(vertical = 24.dp, horizontal = 12.dp),
                verticalArrangement = Arrangement.spacedBy(3.dp)
            ) {
                for (drawerButtons in drawerButtonGroups) {
                    item {
                        Spacer(Modifier.padding(vertical = 5.dp))
                    }

                    itemsIndexed(drawerButtons) { index, drawerButton ->
                        DrawerGroupItem(drawerButton, index to drawerButtons.size)
                    }
                }
            }
        }
    }
}

@Composable
private fun DrawerGroupItem(drawerButton: DrawerButton, groupEntry: Pair<Int, Int>) {
    val (index, groupSize) = groupEntry

    val colors = MaterialTheme.colorScheme
    val textTheme = MaterialTheme.typography

    Row(
        Modifier
            .fillMaxWidth()
            .clip(groupedRoundShape(index, groupSize))
            .background(colors.surfaceContainer)
            .combinedClickable(
                onClick = {
                    drawerButton.onClick?.invoke()
                },
            )
            .padding(vertical = 12.dp, horizontal = 16.dp),
        horizontalArrangement = Arrangement.spacedBy(20.dp),
        verticalAlignment = Alignment.CenterVertically
    ) {
        Icon(
            painterResource(drawerButton.icon),
            drawerButton.label,
            Modifier.size(26.dp),
            tint = colors.onSurface
        )
        Text(
            drawerButton.label, style = avgSizeInStyle(
                textTheme.labelLargeEmphasized, textTheme.bodyLargeEmphasized
            ), color = colors.onBackground
        )
    }
}