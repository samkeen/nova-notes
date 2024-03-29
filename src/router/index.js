import {createRouter, createWebHistory} from "vue-router";
import HomeView from "../views/NotesList.vue";

const router = createRouter({
    history: createWebHistory(import.meta.env.BASE_URL),
    routes: [
        {
            path: "/",
            name: "Home",
            component: HomeView,
        },
        {
            path: "/note",
            name: "NewNote",
            // route level code-splitting (this gives us lazy loading so not all routes are loaded at once)
            component: () => import("../views/Editor.vue"),
        },
        {
            path: "/note/:id",
            name: "EditNote",
            component: () => import("../views/Editor.vue")
        },
        {
            path: "/admin",
            name: "Admin",
            component: () => import("../views/Admin.vue")
        },
        {
            path: "/mdtest",
            name: "MdTest",
            component: () => import("../views/MdTest.vue")
        },
    ],
});
export default router;
