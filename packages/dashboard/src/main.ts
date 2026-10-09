import { mount } from "svelte";
import App from "./App.svelte";
import { theme } from "./lib/theme.svelte.js";
import "./app.css";

theme.apply();
export default mount(App, { target: document.getElementById("app")! });
