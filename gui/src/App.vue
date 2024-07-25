<template>
    <input type="text" v-model="name" />
    <button @click="say_hello">say hello</button>
    <p>{{ greeting }}</p>
    <select v-model="selected">
        <option disabled value="">Please select one</option>
        <option>A</option>
        <option>B</option>
        <option>C</option>
    </select>
</template>

<script setup lang="ts">
import { ref } from "vue";
import { invoke } from "@tauri-apps/api";

const greeting = ref("");
const name = ref("");
const selected = ref("A");

function say_hello() {
    invoke<string>("greet", { name: name.value }).then((response) => {
        greeting.value = response;
    });
}
</script>

<style>
#app {
    font-family: Avenir, Helvetica, Arial, sans-serif;
    -webkit-font-smoothing: antialiased;
    -moz-osx-font-smoothing: grayscale;
    text-align: center;
    color: #2c3e50;
    margin-top: 60px;
}
</style>
