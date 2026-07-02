package com.promtuz.chat.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.*
import androidx.compose.ui.input.nestedscroll.*
import androidx.compose.ui.unit.*
import com.promtuz.chat.presentation.viewmodel.SavedUsersVM
import com.promtuz.chat.ui.components.Avatar
import com.promtuz.chat.ui.components.FlexibleScreen
import com.promtuz.chat.ui.util.groupedRoundShape
import org.koin.androidx.compose.koinViewModel

@Composable
fun SavedUsersScreen(viewModel: SavedUsersVM = koinViewModel()) {
    val isLoading by viewModel.isLoading.collectAsState()

    FlexibleScreen({ Text("Saved Users") }) { padding, scrollBehavior ->
        if (isLoading) {
            Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                LoadingIndicator(Modifier.fillMaxSize(0.25f))
            }
        } else {
            val userGroups by viewModel.users.collectAsState(emptyMap())

            println("USER_GROUPS : $userGroups")

            LazyColumn(
                Modifier
                    .fillMaxSize()
                    .padding(padding)
                    .padding(horizontal = 18.dp)
                    .nestedScroll(scrollBehavior.nestedScrollConnection),
                verticalArrangement = Arrangement.spacedBy(4.dp)
            ) {
//                TODO: reimpl in libcore
//                for ((groupTitle, users) in userGroups) {
//                    item {
//                        Text(groupTitle, Modifier.padding(top = 10.dp, bottom = 6.dp))
//                    }
//                    itemsIndexed(users) { index, user ->
//                        SavedUsersGroup(Modifier, index, user, users.size)
//                    }
//                }
            }
        }
    }
}
//
//@Composable
//private fun SavedUsersGroup(modifier: Modifier, index: Int, user: Any, size: Int) {
//    val interactionSource = remember { MutableInteractionSource() }
//
//    val colors = MaterialTheme.colorScheme
//    val textTheme = MaterialTheme.typography
//
//    Row(
//        modifier
//            .fillMaxWidth()
//            .clip(groupedRoundShape(index, size))
//            .background(colors.surfaceContainer.copy(0.75f))
//            .combinedClickable(
//                interactionSource = interactionSource,
//                indication = ripple(color = colors.surfaceContainerHighest),
//                onClick = {},
//                onLongClick = {}
//            )
//            .padding(vertical = 8.dp, horizontal = 10.dp),
//        horizontalArrangement = Arrangement.spacedBy(12.dp),
//        verticalAlignment = Alignment.CenterVertically
//    ) {
//        Avatar(user.nickname, size = 38.dp)
//
//        Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
//            Row(
//                Modifier.fillMaxWidth(),
//                horizontalArrangement = Arrangement.SpaceBetween,
//                verticalAlignment = Alignment.Top
//            ) {
//                Text(
//                    user.nickname,
//                    style = textTheme.titleMediumEmphasized,
//                    color = colors.onSecondaryContainer
//                )
//            }
//        }
//    }
//}