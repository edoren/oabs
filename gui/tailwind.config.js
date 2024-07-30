/** @type {import('tailwindcss').Config} */
module.exports = {
    // darkMode: ['selector'],
    content: ["./index.html", "./src/**/*.rs"],
    theme: {
        extend: {},
    },
    plugins: [require("@tailwindcss/forms")],
};
